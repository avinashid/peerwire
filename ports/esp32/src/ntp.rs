//! SNTP packets (RFC 4330): a client's ask, and the checks its answer must pass. Std only, so the tests run on any
//! host: `rustc --edition 2021 --test src/ntp.rs -o /tmp/ntp && /tmp/ntp`.

use std::time::Duration;

/// Seconds from 1900, where NTP's era 0 starts, to 1970.
const NTP_TO_UNIX: u64 = 2_208_988_800;

/// 2023-11-14: no answer before it is believed, and a clock before it was never set.
pub const SET_AFTER: u64 = 1_700_000_000;

/// A client's ask: no leap warning, version 4, mode 3, and `token` as its transmit time, which the server's answer
/// must echo as its originate time. A random token is what keeps a sender off the path from answering.
pub fn ask(token: [u8; 8]) -> [u8; 48] {
    let mut ask = [0u8; 48];
    ask[0] = 0x23;
    ask[40..].copy_from_slice(&token);
    ask
}

/// Why an answer gave no time.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not a server's answer to our ask (short, not mode 4, or without our token): wait on for ours.
    NotOurs,
    /// Our answer, with no time to take, and why.
    NoTime(&'static str),
}

/// The time since 1970 that `answer` gives, if it answers the ask that carried `token`; else why not.
pub fn time(answer: &[u8], token: &[u8; 8]) -> Result<Duration, Refused> {
    let Some(answer) = answer.get(..48) else { return Err(Refused::NotOurs) };
    if answer[0] & 7 != 4 || answer[24..32] != token[..] {
        return Err(Refused::NotOurs);
    }
    // Both mean the server has no time to give: unsynchronised, or a kiss-o'-death.
    if answer[0] >> 6 == 3 {
        return Err(Refused::NoTime("server unsynchronised (leap indicator 3)"));
    }
    match answer[1] {
        0 => return Err(Refused::NoTime("kiss-o'-death (stratum 0)")),
        16.. => return Err(Refused::NoTime("server unsynchronised (stratum 16 or more)")),
        _ => {}
    }
    let word = |i: usize| u64::from(u32::from_be_bytes([answer[i], answer[i + 1], answer[i + 2], answer[i + 3]]));
    let (secs, frac) = (word(40), word(44));
    if secs == 0 && frac == 0 {
        return Err(Refused::NoTime("no transmit time"));
    }
    // RFC 4330 section 3: with the top bit clear the seconds count from 2036-02-07 06:28:16 UTC, NTP's era 1.
    let ntp = if secs >> 31 == 0 { secs + (1 << 32) } else { secs };
    // Checked: era 0's seconds from 1 << 31 are 1968 and 1969, before 1970 (NB103-I1).
    let Some(unix) = ntp.checked_sub(NTP_TO_UNIX).filter(|&unix| unix >= SET_AFTER) else {
        return Err(Refused::NoTime("a time before 2023-11-14"));
    };
    Ok(Duration::from_secs(unix) + Duration::from_nanos((frac * 1_000_000_000) >> 32))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    /// A stratum-2 server's answer to `TOKEN`, giving `secs` and `frac` as its transmit time.
    fn answer(secs: u32, frac: u32) -> [u8; 48] {
        let mut a = [0u8; 48];
        a[0] = 0x24;
        a[1] = 2;
        a[24..32].copy_from_slice(&TOKEN);
        a[40..44].copy_from_slice(&secs.to_be_bytes());
        a[44..48].copy_from_slice(&frac.to_be_bytes());
        a
    }

    #[test]
    fn ask_carries_the_token() {
        let a = ask(TOKEN);
        assert_eq!(a[0], 0x23);
        assert_eq!(a[40..], TOKEN);
        assert!(a[1..40].iter().all(|&b| b == 0));
    }

    #[test]
    fn era_0() {
        // 2026-11-05 00:00:00 UTC, and half a second.
        let unix = 1_793_836_800u64;
        let t = time(&answer((unix + NTP_TO_UNIX) as u32, 1 << 31), &TOKEN).unwrap();
        assert_eq!(t, Duration::from_secs(unix) + Duration::from_millis(500));
    }

    #[test]
    fn era_1() {
        // 100 s into era 1: 2036-02-07 06:29:56 UTC.
        let t = time(&answer(100, 0), &TOKEN).unwrap();
        assert_eq!(t.as_secs(), (1 << 32) + 100 - NTP_TO_UNIX);
        assert_eq!(t.as_secs(), 2_085_978_596);
    }

    #[test]
    fn refusals() {
        // 2026-11-05.
        let good = answer(4_002_825_600, 0);
        assert!(time(&good, &TOKEN).is_ok());
        let refused = |f: &dyn Fn(&mut [u8; 48]), why: Refused| {
            let mut a = good;
            f(&mut a);
            assert_eq!(time(&a, &TOKEN), Err(why));
        };
        refused(&|a| a[0] = 0x23, Refused::NotOurs);
        refused(&|a| a[31] ^= 1, Refused::NotOurs);
        refused(&|a| a[0] = 0xe4, Refused::NoTime("server unsynchronised (leap indicator 3)"));
        refused(&|a| a[1] = 0, Refused::NoTime("kiss-o'-death (stratum 0)"));
        refused(&|a| a[1] = 16, Refused::NoTime("server unsynchronised (stratum 16 or more)"));
        refused(&|a| a[1] = 255, Refused::NoTime("server unsynchronised (stratum 16 or more)"));
        refused(&|a| a[40..48].fill(0), Refused::NoTime("no transmit time"));
        // 2020-01-01: era 0, before 2023-11-14.
        let old = 3_786_825_600u32.to_be_bytes();
        refused(&|a| a[40..44].copy_from_slice(&old), Refused::NoTime("a time before 2023-11-14"));
        // Era 0's first and last seconds before 1970 (1968-01-20 and 1969-12-31): unchecked, they wrap (NB103-I1).
        for secs in [1u32 << 31, NTP_TO_UNIX as u32 - 1] {
            let early = secs.to_be_bytes();
            refused(&|a| a[40..44].copy_from_slice(&early), Refused::NoTime("a time before 2023-11-14"));
        }
        assert_eq!(time(&good[..47], &TOKEN), Err(Refused::NotOurs));
        // A leap second announced (leap indicator 1) is still a time.
        let mut leap = good;
        leap[0] = 0x64;
        assert!(time(&leap, &TOKEN).is_ok());
    }
}
