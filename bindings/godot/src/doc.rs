use crate::peer::PeerwireMultiplayerPeer;
use crate::session::Session;
use godot::classes::{Json, RefCounted};
use godot::global::Error;
use godot::prelude::*;
use peerwire_sync::{Change, DocId, DocMut, Item, Kind, Source, State, Status};

/// A shared document: maps, sets and logs every peer of the game holds, that converge, as `room.doc()` of
/// `@peerwire/js`. Open it with `PeerwireMultiplayerPeer.open_doc(name)`; the same name on every peer is the same
/// doc. Joiners link to the host alone, so the host passes on what each writes; one that joins or opens the doc
/// late catches up from it.
///
/// ```gdscript
/// var doc := peer.open_doc("board")
/// doc.changed.connect(func(e): print(e.name, " ", e.get("key", e.get("id")), " = ", e.get("value")))
/// doc.map_set("meta", "title", "Plans")        # a map key, the latest write wins
/// var id := doc.set_add("strokes", {"pts": [1, 2]})
/// doc.log_append("chat", {"text": "hi"})       # a log keeps its newest 1000 (log_keep)
/// ```
///
/// Values are JSON: what `JSON.stringify` writes of them is what every peer reads back (numbers as floats).
/// Writes return `false` (or `""`) when peers filled the doc (10000 entries or 4 MiB of JSON). `changed` and
/// `sync_changed` fire as `poll()` runs.
#[derive(GodotClass)]
#[class(base = RefCounted, no_init)]
pub struct PeerwireDoc {
    base: Base<RefCounted>,
    peer: InstanceId,
    id: DocId,
}

fn put(d: &mut VarDictionary, key: &str, value: Variant) {
    d.set(&key.to_variant(), &value);
}

fn json(v: &Variant) -> String {
    Json::stringify(v).to_string()
}

fn value(text: &str) -> Variant {
    Json::parse_string(text)
}

fn item_dict(kind: Kind, it: &Item) -> VarDictionary {
    let mut d = VarDictionary::new();
    put(&mut d, if kind == Kind::Map { "key" } else { "id" }, it.key.to_variant());
    put(&mut d, "value", value(&it.value));
    put(&mut d, "from", hex(&it.from.0).to_variant());
    d
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A `changed` event as a Dictionary: `name`, `type` ("map", "set" or "log"), `key` (a map's) or `id`, `value` (or
/// `deleted: true`), `from` (the writer's public key, hex) and `local`.
pub(crate) fn change_dict(c: &Change) -> VarDictionary {
    let mut d = VarDictionary::new();
    put(&mut d, "name", c.collection().to_variant());
    put(&mut d, "type", c.kind().name().to_variant());
    put(&mut d, if c.kind() == Kind::Map { "key" } else { "id" }, c.key().to_variant());
    match c.value() {
        Some(v) => put(&mut d, "value", value(v)),
        None => put(&mut d, "deleted", true.to_variant()),
    }
    put(&mut d, "from", hex(&c.from().0).to_variant());
    put(&mut d, "local", c.local().to_variant());
    d
}

/// A `sync_changed` status as a Dictionary: `state` ("alone", "loading" or "synced"), `from` (hex, or ""),
/// `have` and `total` (the entries of the answer under way), `source` ("members" once caught up, else "") and
/// `at` (ms, for a save on the relays, which Godot doesn't keep: 0).
pub(crate) fn status_dict(s: &Status) -> VarDictionary {
    let mut d = VarDictionary::new();
    let state = match s.state {
        State::Alone => "alone",
        State::Loading => "loading",
        State::Synced => "synced",
    };
    put(&mut d, "state", state.to_variant());
    put(&mut d, "from", s.from.map_or(String::new(), |p| hex(&p.0)).to_variant());
    put(&mut d, "have", (s.have as i64).to_variant());
    put(&mut d, "total", (s.total as i64).to_variant());
    let source = match s.source {
        Some(Source::Members) => "members",
        Some(Source::Relays) => "relays",
        None => "",
    };
    put(&mut d, "source", source.to_variant());
    put(&mut d, "at", (s.at.unwrap_or(0) as i64).to_variant());
    d
}

impl PeerwireDoc {
    /// Doc `id` of the multiplayer peer `peer`.
    pub(crate) fn make(peer: InstanceId, id: DocId) -> Gd<PeerwireDoc> {
        Gd::from_init_fn(|base| PeerwireDoc { base, peer, id })
    }

    /// Runs `f` on the doc, if its peer still has it open.
    fn with<R>(&self, f: impl FnOnce(DocMut<'_>) -> R) -> Option<R> {
        let mut peer = Gd::<PeerwireMultiplayerPeer>::try_from_instance_id(self.peer).ok()?;
        let mut p = peer.bind_mut();
        let s: &mut Session = p.session_mut()?;
        Some(f(s.doc(self.id)?))
    }

    fn entries(&self, kind: Kind, coll: GString) -> VarArray {
        let items = self.with(|d| d.entries(kind, &coll.to_string())).unwrap_or_default();
        items.iter().map(|it| item_dict(kind, it).to_variant()).collect()
    }

    fn get(&self, kind: Kind, coll: GString, key: GString) -> Variant {
        self.with(|d| d.get(kind, &coll.to_string(), &key.to_string()).map(value)).flatten().unwrap_or_default()
    }
}

#[godot_api]
impl PeerwireDoc {
    /// A key or item changed, by a write here or a peer's (see `change_dict`).
    #[signal]
    fn changed(event: VarDictionary);

    /// Where catching up stands changed (see `get_sync`).
    #[signal]
    fn sync_changed(status: VarDictionary);

    /// Sets map `coll`'s `key` to `value`.
    #[func]
    fn map_set(&self, coll: GString, key: GString, value: Variant) -> bool {
        let r = self.with(|mut d| d.map_set(&coll.to_string(), &key.to_string(), &json(&value)));
        if let Some(Err(e)) = r {
            godot_error!("PeerwireDoc.map_set: {e}");
        }
        matches!(r, Some(Ok(true)))
    }

    /// What map `coll`'s `key` holds, or null.
    #[func]
    fn map_get(&self, coll: GString, key: GString) -> Variant {
        self.get(Kind::Map, coll, key)
    }

    /// Deletes map `coll`'s `key`: whether it held a value.
    #[func]
    fn map_erase(&self, coll: GString, key: GString) -> bool {
        matches!(self.with(|mut d| d.map_delete(&coll.to_string(), &key.to_string())), Some(Ok(true)))
    }

    /// Map `coll`'s keys in order, each `{key, value, from}`.
    #[func]
    fn map_entries(&self, coll: GString) -> VarArray {
        self.entries(Kind::Map, coll)
    }

    /// Adds `value` to set `coll`: its id, or "" if the doc refused it.
    #[func]
    fn set_add(&self, coll: GString, value: Variant) -> GString {
        let r = self.with(|mut d| d.set_add(&coll.to_string(), &json(&value)));
        if let Some(Err(e)) = r {
            godot_error!("PeerwireDoc.set_add: {e}");
        }
        GString::from(r.and_then(Result::ok).flatten().unwrap_or_default().as_str())
    }

    #[func]
    fn set_get(&self, coll: GString, id: GString) -> Variant {
        self.get(Kind::Set, coll, id)
    }

    /// Deletes item `id` of set `coll`: whether it held it.
    #[func]
    fn set_erase(&self, coll: GString, id: GString) -> bool {
        matches!(self.with(|mut d| d.set_delete(&coll.to_string(), &id.to_string())), Some(Ok(true)))
    }

    /// Set `coll`'s items, oldest first, each `{id, value, from}`.
    #[func]
    fn set_entries(&self, coll: GString) -> VarArray {
        self.entries(Kind::Set, coll)
    }

    /// Appends `value` to log `coll`: its id, or "" if the doc refused it.
    #[func]
    fn log_append(&self, coll: GString, value: Variant) -> GString {
        let r = self.with(|mut d| d.log_append(&coll.to_string(), &json(&value)));
        if let Some(Err(e)) = r {
            godot_error!("PeerwireDoc.log_append: {e}");
        }
        GString::from(r.and_then(Result::ok).flatten().unwrap_or_default().as_str())
    }

    /// Log `coll`'s entries, oldest first, each `{id, value, from}`.
    #[func]
    fn log_entries(&self, coll: GString) -> VarArray {
        self.entries(Kind::Log, coll)
    }

    /// How many entries log `coll` keeps, the newest (1 to 10000, default 1000): every peer should say the same,
    /// right after opening the doc.
    #[func]
    fn log_keep(&self, coll: GString, keep: i64) -> Error {
        match self.with(|mut d| d.log_keep(&coll.to_string(), keep.max(0) as u64)) {
            Some(Ok(())) => Error::OK,
            Some(Err(_)) => Error::ERR_INVALID_PARAMETER,
            None => Error::ERR_UNAVAILABLE,
        }
    }

    /// Where catching up stands: `{state, from, have, total, source, at}` (see `status_dict`).
    #[func]
    fn get_sync(&self) -> VarDictionary {
        self.with(|d| status_dict(&d.sync())).unwrap_or_default()
    }

    /// The doc as a copy to keep on this device, for `open_doc(name, copy)` when the game starts again. It holds
    /// what peers wrote: keep it as private as the game.
    #[func]
    fn snapshot(&self) -> PackedByteArray {
        self.with(|d| PackedByteArray::from(d.snapshot().as_slice())).unwrap_or_default()
    }

    /// Stops syncing and forgets the doc: opening it again catches up afresh.
    #[func]
    fn close(&self) {
        let Ok(mut peer) = Gd::<PeerwireMultiplayerPeer>::try_from_instance_id(self.peer) else { return };
        peer.bind_mut().close_doc(self.id);
    }
}
