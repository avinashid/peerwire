//! The Unicode a file name is cleaned with (files.js `cleanName`): NFC, as `String.prototype.normalize('NFC')`
//! makes it, and the format characters, `\p{Cf}`. Both come from tables that `scripts/unicode-tables.mjs` reads off
//! Node's own Unicode data (`unicode_tables.rs`, whose header names the version), so a name from a peer cleans to
//! the same one here as in a browser of that Unicode version, with no crate to keep in step with it.
//!
//! NFC is the standard's (UAX #15): each character decomposed in full, the non-starters between two starters sorted
//! by canonical combining class, then each character composed with the last starter before it if nothing between
//! blocks it. Hangul syllables are decomposed and composed by rule (the Unicode Standard, 3.12), as no table lists
//! them. A name is at most `MAX_RAW_NAME` characters, so binary searches in the tables are fast enough. Memory is
//! linear in the name, and so is the work but for sorting each run of non-starters; nothing recurses.

include!("unicode_tables.rs");

// Hangul syllables: 19 leading consonants (L) by 21 vowels (V) by 28 trailing consonants (T), the first T none.
const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// Whether `c` is a format character (general category Cf), as JS `/\p{Cf}/u` has it.
pub(crate) fn is_format(c: char) -> bool {
    let c = u32::from(c);
    let i = FORMAT.partition_point(|&(first, _)| first <= c);
    i > 0 && c <= FORMAT[i - 1].1
}

/// `s` in NFC, as JS `s.normalize('NFC')` makes it.
pub(crate) fn nfc(s: &str) -> String {
    if s.chars().all(|c| c < NFC_BELOW) {
        return s.to_owned();
    }
    // Each character with its class.
    let mut v = Vec::with_capacity(s.len());
    for c in s.chars() {
        decompose(u32::from(c), &mut v);
    }
    // Canonical ordering: each run of non-starters sorted by class, stably.
    for run in v.split_mut(|&(_, class)| class == 0) {
        run.sort_by_key(|&(_, class)| class);
    }
    compose(&mut v);
    v.into_iter().filter_map(|(c, _)| char::from_u32(c)).collect()
}

/// Pushes the canonical decomposition of `c`, each character with its class.
fn decompose(c: u32, out: &mut Vec<(u32, u8)>) {
    let s = c.wrapping_sub(S_BASE);
    if s < S_COUNT {
        out.push((L_BASE + s / N_COUNT, 0));
        out.push((V_BASE + (s % N_COUNT) / T_COUNT, 0));
        if !s.is_multiple_of(T_COUNT) {
            out.push((T_BASE + s % T_COUNT, 0));
        }
        return;
    }
    // A pair's second never decomposes, nor does a singleton's target, so only the first is followed down, each step
    // a character shorter; the seconds met on the way come out last first.
    let at = out.len();
    let mut c = c;
    while let Some((a, b)) = pair(c) {
        out.push((b, class(b)));
        c = a;
    }
    let c = single(c).unwrap_or(c);
    out.push((c, class(c)));
    out[at..].reverse();
}

/// Canonical composition, in place: each character composed with the last starter before it, unless one between
/// blocks it, by being a starter or of a class not below its own.
fn compose(v: &mut Vec<(u32, u8)>) {
    let mut kept = 0;
    let mut starter: Option<usize> = None;
    for i in 0..v.len() {
        let (c, class) = v[i];
        if let Some(s) = starter {
            // Those kept after the starter are non-starters in order of class: the last is the one to ask.
            if kept == s + 1 || v[kept - 1].1 < class {
                if let Some(composed) = composite(v[s].0, c) {
                    v[s].0 = composed;
                    continue;
                }
            }
        }
        if class == 0 {
            starter = Some(kept);
        }
        v[kept] = (c, class);
        kept += 1;
    }
    v.truncate(kept);
}

/// What NFC composes `a` and `b` to, if anything.
fn composite(a: u32, b: u32) -> Option<u32> {
    let (l, v) = (a.wrapping_sub(L_BASE), b.wrapping_sub(V_BASE));
    if l < L_COUNT && v < V_COUNT {
        return Some(S_BASE + (l * V_COUNT + v) * T_COUNT);
    }
    let (s, t) = (a.wrapping_sub(S_BASE), b.wrapping_sub(T_BASE));
    if s < S_COUNT && s.is_multiple_of(T_COUNT) && (1..T_COUNT).contains(&t) {
        return Some(a + t);
    }
    // The pair's place in PAIRS, then the character at that place in the last run starting at or before it.
    let i = COMPOSITES[COMPOSITES.binary_search_by(|&i| PAIRS[usize::from(i)].cmp(&(a, b))).ok()?];
    let (first, at) = PAIR_RUNS[PAIR_RUNS.partition_point(|&(_, at)| at <= i).checked_sub(1)?];
    Some(first + u32::from(i - at))
}

/// `c`'s canonical combining class, as a rank (`CLASSES`): 0 for a starter.
fn class(c: u32) -> u8 {
    find(&CLASS_RUNS, CLASSES.len(), c).map_or(0, |i| CLASSES[i])
}

/// What `c` decomposes to, if that is one other character.
fn single(c: u32) -> Option<u32> {
    find(&SINGLE_RUNS, SINGLES.len(), c).map(|i| SINGLES[i]).filter(|&t| t != 0)
}

/// `c`'s pair (a, b), if it decomposes to more than one character: its decomposition is a's, then b.
fn pair(c: u32) -> Option<(u32, u32)> {
    find(&PAIR_RUNS, PAIRS.len(), c).map(|i| PAIRS[i]).filter(|&(a, _)| a != 0)
}

/// Where `c` is in a table of `len` entries kept in runs (`runs`: each one's first character and where in the table
/// it starts), if a run holds it.
fn find(runs: &[(u32, u16)], len: usize, c: u32) -> Option<usize> {
    let r = runs.partition_point(|&(first, _)| first <= c).checked_sub(1)?;
    let (first, at) = runs[r];
    let end = runs.get(r + 1).map_or(len, |&(_, next)| usize::from(next));
    let i = usize::from(at) + (c - first) as usize;
    (i < end).then_some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    // What Node 26 (Unicode 17.0) makes of each.
    #[test]
    fn nfc_as_javascript_makes_it() {
        for (s, want) in [
            ("file.txt", "file.txt"),
            ("Cafe\u{301} \u{212B}.txt", "Caf\u{E9} \u{C5}.txt"),
            // Reordered by class, then composed past a lower class but not past an equal one.
            ("a\u{307}\u{323}", "\u{1EA1}\u{307}"),
            ("A\u{30A}\u{327}", "\u{C5}\u{327}"),
            ("a\u{301}\u{301}", "\u{E1}\u{301}"),
            ("\u{1E0B}\u{323}", "\u{1E0D}\u{307}"),
            ("\u{301}a", "\u{301}a"),
            // Hangul by rule.
            ("\u{1100}\u{1161}\u{11A8}", "\u{AC01}"),
            ("\u{AC00}\u{11A8}", "\u{AC01}"),
            ("\u{AC01}\u{11A8}", "\u{AC01}\u{11A8}"),
            ("\u{1100}\u{1161}\u{301}\u{11A8}", "\u{AC00}\u{301}\u{11A8}"),
            // Singletons, an exclusion and a non-starter's decomposition, never composed back.
            ("\u{2126}\u{F900}", "\u{3A9}\u{8C48}"),
            ("\u{958}", "\u{915}\u{93C}"),
            ("\u{344}", "\u{308}\u{301}"),
            ("\u{FB2}\u{F81}", "\u{FB2}\u{F71}\u{F80}"),
            // Two starters compose only side by side.
            ("\u{B47}\u{B3E}", "\u{B4B}"),
            ("\u{B47}\u{300}\u{B3E}", "\u{B47}\u{300}\u{B3E}"),
        ] {
            assert_eq!(nfc(s), want, "{s:?}");
        }
    }

    #[test]
    fn format_characters() {
        for c in ['\u{AD}', '\u{200B}', '\u{202E}', '\u{FEFF}', '\u{1D173}', '\u{E0001}', '\u{E007F}'] {
            assert!(is_format(c), "{c:?}");
        }
        for c in ['a', '\u{0}', '\u{300}', '\u{2028}', '\u{E0080}', '\u{10FFFF}'] {
            assert!(!is_format(c), "{c:?}");
        }
    }
}
