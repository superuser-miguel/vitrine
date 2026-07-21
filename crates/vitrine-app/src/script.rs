//! The Lua scripting host — PLAN §16.2 (the contract) and §16.7 (what
//! "sandboxed" means here).
//!
//! **Scripts are trusted user configuration, not untrusted plugins**
//! (§16.7.1). A script runs in-process with the app's full authority, and the
//! author-facing docs say so plainly. Nothing in this module is armour against
//! a hostile script; it is all guardrail against a well-meaning author's
//! mistake. What the guardrails buy is that the reachable accidents — an
//! infinite loop, a runaway allocation, a syntax error, a stray `os.execute` —
//! surface as a toast naming the script instead of hanging the sort worker or
//! taking the process down with it.
//!
//! Three limits, all required by §16.6's E1 acceptance criteria:
//!
//! 1. **The standard library is opted *into*, not stripped out.** §16.2
//!    describes the sandbox as "subtractive" — deleting `os`, `io`, `require`
//!    and friends from globals. Declining to *load* them is strictly stronger:
//!    a deleted global can still be reached through an upvalue or the registry,
//!    whereas an unloaded library has no table to find. We load `table`,
//!    `string` and `math` and nothing else. The base library is always present
//!    (Lua has no flag for it), so its loaders are removed explicitly below —
//!    that part really is subtractive, and it is the weakest link here.
//! 2. **Text chunks only** (`ChunkMode::Text`). Lua's VM does not validate
//!    bytecode; loading a crafted binary chunk is a known escape hatch. Even
//!    for trusted scripts this is worth refusing, because a *binary* file in
//!    the scripts directory is far more likely to be an accident than an
//!    intention.
//! 3. **Instruction and memory ceilings.** `while true do end` in a sort key
//!    is reachable by a competent author having a bad afternoon; without the
//!    instruction hook it hangs the worker forever with no diagnostic.
//!
//! The comparator stays native (§16.2): a script supplies a *key function*,
//! called once per item and memoised by the caller, never a comparator called
//! O(n log n) times.

use mlua::chunk::ChunkMode;
use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Table, Value, VmState};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard};

/// The API version handed to scripts as `vitrine.api_version` (§16.2).
/// Additions are fine without a bump; removals and behaviour changes are not.
pub const API_VERSION: u32 = 1;

/// VM instructions between hook firings. The hook itself is cheap but not
/// free, and mlua warns that a low stride "can incur a very high overhead" —
/// this is coarse enough to stay off the profile and fine enough that a
/// runaway loop dies in milliseconds, not seconds.
const HOOK_STRIDE: u32 = 20_000;

/// How many times the hook may fire within one script call before we call it a
/// runaway. 50 × 20_000 = one million instructions, which is lavish for a sort
/// key over a single item and still bounded.
const MAX_HOOK_FIRES: u32 = 50;

/// Ceiling on the Lua heap. Generous for key functions and memo tables; small
/// enough that a runaway allocation errors instead of pushing the app into
/// swap. This is a *guardrail* number, not a tuned one.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// A failure attributable to one script, carrying enough to name it in a toast
/// (§16.6: "a script error surfaces as a toast naming the script, never a
/// crash").
#[derive(Debug, Clone)]
pub struct ScriptError {
    /// The script's file stem, e.g. `natural-sort`.
    pub script: String,
    pub message: String,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.script, self.message)
    }
}

/// The facts a script may read about one item (§16.2's read side). Plain data,
/// assembled by the caller from an `ImageObject` — nothing here requires I/O at
/// call time, which is what lets a key function stay off the hot path.
///
/// `width`/`height`/`camera` from §16.2 are not yet carried by `ImageObject`;
/// they arrive with the enrichment callback that also unblocks Date Taken
/// (V-12) and are additive when they do.
#[derive(Debug, Clone, Default)]
pub struct ItemFacts {
    pub name: String,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub content_type: String,
    pub content_hash: String,
    pub rating: i32,
    pub orientation: i32,
    pub date_taken: Option<i64>,
}

/// What a key function may return. Anything else is a script error, reported
/// with the script's name rather than silently coerced — a key that
/// accidentally returns `nil` should be loud, not sort everything equal.
#[derive(Debug, Clone, PartialEq)]
pub enum SortKey {
    Num(f64),
    Str(String),
}

impl SortKey {
    /// Total order across both variants. Numbers sort before strings so a
    /// script that returns mixed types still produces a *stable, explicable*
    /// order instead of an arbitrary one; NaN sorts last for the same reason.
    pub fn cmp_key(&self, other: &SortKey) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.partial_cmp(b).unwrap_or_else(|| {
                // NaN: push it to the end rather than declaring equality,
                // which would make the sort unstable.
                match (a.is_nan(), b.is_nan()) {
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    _ => Ordering::Equal,
                }
            }),
            (SortKey::Str(a), SortKey::Str(b)) => a.cmp(b),
            (SortKey::Num(_), SortKey::Str(_)) => Ordering::Less,
            (SortKey::Str(_), SortKey::Num(_)) => Ordering::Greater,
        }
    }
}

/// A sort order registered by a script via `vitrine.register_sort`.
pub struct SortProvider {
    /// User-visible label for the Sort By menu.
    pub name: String,
    /// The script that registered it — for error attribution and for unloading
    /// every provider belonging to one file on reload.
    pub script: String,
    key: Function,
}

/// The Lua host. Owns one VM and is `Send`, so it can be handed to a worker —
/// §16.2 forbids scripts from running anywhere near the main loop.
///
/// `Send` costs mlua's `send` feature (a pure `cfg` flag, no extra crates).
/// Without it `Lua` is emphatically *not* `Send`, whatever docs.rs suggests —
/// docs.rs builds with all features enabled. The price of the feature is that
/// every callback registered into Lua must be `Send`, which is why the hook
/// counter is an atomic and the provider list is a `Mutex` rather than the
/// `Cell`/`RefCell` this would otherwise want.
///
/// None of that is on the hot path: the comparator never enters Lua at all
/// (§16.2 — key functions, computed once per item and memoised by the caller).
pub struct ScriptHost {
    lua: Lua,
    /// Hook firings within the current call. Reset before every entry into
    /// Lua; read by the hook to decide when a call has run away.
    fires: Arc<AtomicU32>,
    providers: Arc<Mutex<Vec<SortProvider>>>,
}

impl ScriptHost {
    /// Build a VM with the sandbox in place. Fails only if Lua itself cannot
    /// be initialised.
    pub fn new() -> Result<ScriptHost, ScriptError> {
        let err = |e: mlua::Error| ScriptError {
            script: "<host>".into(),
            message: e.to_string(),
        };

        // Limit 1: opt in. No IO, no OS, no PACKAGE (hence no `require`
        // machinery), no DEBUG (which can reach upvalues and defeat the rest),
        // no COROUTINE (a key function has no use for it, and it complicates
        // the instruction budget by giving a script more than one stack).
        let libs = StdLib::TABLE | StdLib::STRING | StdLib::MATH;
        let lua = Lua::new_with(libs, LuaOptions::default()).map_err(err)?;

        // Limit 3a: memory.
        lua.set_memory_limit(MEMORY_LIMIT).map_err(err)?;

        // Limit 3b: instructions. The hook is per-VM and stays installed; the
        // budget is per-call, reset by `enter`.
        let fires = Arc::new(AtomicU32::new(0));
        {
            let fires = fires.clone();
            lua.set_hook(
                HookTriggers {
                    every_nth_instruction: Some(HOOK_STRIDE),
                    ..Default::default()
                },
                move |_lua, _debug| {
                    let n = fires.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                    if n > MAX_HOOK_FIRES {
                        // Surfaces as a normal Lua error, so it travels the
                        // same path as a syntax error and lands in a toast.
                        Err(mlua::Error::runtime(
                            "script ran too long (instruction budget exhausted) \
                             — check for an unbounded loop",
                        ))
                    } else {
                        Ok(VmState::Continue)
                    }
                },
            )
            .map_err(err)?;
        }

        // The base library is always loaded and has no StdLib flag, so its
        // code loaders are removed by hand. This is the one genuinely
        // subtractive step and therefore the weakest part of the sandbox —
        // which is precisely why §16.7 declines to call any of this a security
        // boundary.
        let globals = lua.globals();
        for name in [
            "load",
            "loadstring",
            "loadfile",
            "dofile",
            "require",
            "collectgarbage",
        ] {
            globals.set(name, Value::Nil).map_err(err)?;
        }

        let providers = Arc::new(Mutex::new(Vec::<SortProvider>::new()));

        // The `vitrine` table — the world, per §16.2.
        let vitrine = lua.create_table().map_err(err)?;
        vitrine.set("api_version", API_VERSION).map_err(err)?;
        globals.set("vitrine", &vitrine).map_err(err)?;
        globals.set("_G", Value::Nil).map_err(err)?;

        Ok(ScriptHost {
            lua,
            fires,
            providers,
        })
    }

    /// Reset the per-call instruction budget. Every entry into Lua goes
    /// through this, so one runaway call cannot poison the next.
    fn enter(&self) {
        self.fires.store(0, AtomicOrdering::Relaxed);
    }

    /// The provider list, recovering from a poisoned lock instead of
    /// panicking. Poisoning means some earlier call panicked while holding it;
    /// propagating that would turn one bad script into a permanently dead
    /// scripting tier, which is exactly the "never a crash" §16.6 rules out.
    /// The data behind the lock is a plain `Vec` with no invariant a panic
    /// could have left half-broken, so taking it back is safe.
    fn lock(&self) -> MutexGuard<'_, Vec<SortProvider>> {
        self.providers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Load one script's source. `name` is the file stem, used for error
    /// attribution and to scope its registrations.
    ///
    /// Registrations made by a previous load of the same name are dropped
    /// first, which is what makes hot reload idempotent rather than
    /// accumulating a duplicate menu entry per save.
    pub fn load_str(&self, name: &str, src: &str) -> Result<(), ScriptError> {
        self.lock().retain(|p| p.script != name);

        let err = |e: mlua::Error| ScriptError {
            script: name.to_string(),
            message: e.to_string(),
        };

        // `register_sort` is rebound per load so it can attribute providers to
        // the script currently executing without the script naming itself.
        let vitrine: Table = self.lua.globals().get("vitrine").map_err(err)?;
        let providers = self.providers.clone();
        let owner = name.to_string();
        let register = self
            .lua
            .create_function(move |_, spec: Table| {
                let name: String = spec.get("name")?;
                let key: Function = spec.get("key")?;
                if name.trim().is_empty() {
                    return Err(mlua::Error::runtime(
                        "register_sort: `name` must be a non-empty string",
                    ));
                }
                providers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(SortProvider {
                        name,
                        script: owner.clone(),
                        key,
                    });
                Ok(())
            })
            .map_err(err)?;
        vitrine.set("register_sort", register).map_err(err)?;

        self.enter();
        // Limit 2: text only.
        self.lua
            .load(src)
            .set_name(name)
            .set_mode(ChunkMode::Text)
            .exec()
            .map_err(err)?;
        Ok(())
    }

    /// Load every `*.lua` in `dir`, newest errors and all. A bad script does
    /// not prevent the others from loading — one broken file should cost its
    /// own sort order, not the whole tier.
    ///
    /// A missing directory is not an error: no scripts is the normal state.
    pub fn load_dir(&self, dir: &Path) -> Vec<ScriptError> {
        let mut errors = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return errors,
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "lua"))
            .collect();
        // Deterministic order so two scripts registering the same label always
        // resolve the same way across launches.
        paths.sort();

        for path in paths {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<unnamed>".into());
            match std::fs::read_to_string(&path) {
                Ok(src) => {
                    if let Err(e) = self.load_str(&name, &src) {
                        errors.push(e);
                    }
                }
                Err(e) => errors.push(ScriptError {
                    script: name,
                    message: format!("could not be read: {e}"),
                }),
            }
        }
        errors
    }

    /// Every registered sort order, in registration order.
    pub fn providers(&self) -> MutexGuard<'_, Vec<SortProvider>> {
        self.lock()
    }

    /// Compute one item's sort key. Called once per item by the worker and
    /// memoised there; the comparator never reaches Lua.
    pub fn sort_key(&self, index: usize, facts: &ItemFacts) -> Result<SortKey, ScriptError> {
        let providers = self.lock();
        let provider = providers.get(index).ok_or_else(|| ScriptError {
            script: "<host>".into(),
            message: format!("no sort provider at index {index}"),
        })?;
        let script = provider.script.clone();
        let err = move |e: mlua::Error| ScriptError {
            script: script.clone(),
            message: e.to_string(),
        };

        let table = self.facts_table(facts).map_err(err.clone())?;
        self.enter();
        let value: Value = provider.key.call(table).map_err(err.clone())?;
        match value {
            Value::Integer(i) => Ok(SortKey::Num(i as f64)),
            Value::Number(n) => Ok(SortKey::Num(n)),
            Value::String(s) => Ok(SortKey::Str(s.to_string_lossy().to_string())),
            other => Err(ScriptError {
                script: provider.script.clone(),
                message: format!(
                    "sort key returned {}, expected a number or a string",
                    other.type_name()
                ),
            }),
        }
    }

    fn facts_table(&self, facts: &ItemFacts) -> Result<Table, mlua::Error> {
        let t = self.lua.create_table()?;
        t.set("name", facts.name.as_str())?;
        t.set("path", facts.path.as_str())?;
        t.set("size", facts.size)?;
        t.set("mtime", facts.mtime)?;
        t.set("content_type", facts.content_type.as_str())?;
        t.set("content_hash", facts.content_hash.as_str())?;
        t.set("rating", facts.rating)?;
        t.set("orientation", facts.orientation)?;
        match facts.date_taken {
            Some(d) => t.set("date_taken", d)?,
            None => t.set("date_taken", Value::Nil)?,
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: &str) -> ItemFacts {
        ItemFacts {
            name: name.to_string(),
            path: format!("/photos/{name}"),
            ..Default::default()
        }
    }

    fn host_with(src: &str) -> ScriptHost {
        let host = ScriptHost::new().expect("host");
        host.load_str("test", src).expect("load");
        host
    }

    /// E1's headline acceptance criterion (§16.6): a natural-sort script must
    /// order `img_2` before `img_10`, which plain lexicographic ordering gets
    /// backwards.
    #[test]
    fn natural_sort_orders_img_2_before_img_10() {
        let host = host_with(
            r#"
            vitrine.register_sort {
              name = "Natural",
              key = function(item)
                return (item.name:gsub("%d+", function(d)
                  return string.format("%08d", tonumber(d))
                end))
              end,
            }
            "#,
        );
        assert_eq!(host.providers().len(), 1);
        assert_eq!(host.providers()[0].name, "Natural");

        let two = host.sort_key(0, &facts("img_2.jpg")).unwrap();
        let ten = host.sort_key(0, &facts("img_10.jpg")).unwrap();
        assert_eq!(two.cmp_key(&ten), std::cmp::Ordering::Less);

        // The bug the script exists to fix, stated as the control.
        assert!("img_10.jpg" < "img_2.jpg");
    }

    /// §16.6: an unbounded loop must error, not hang the worker.
    #[test]
    fn runaway_loop_errors_instead_of_hanging() {
        let host = host_with(
            r#"
            vitrine.register_sort {
              name = "Spin",
              key = function(item) while true do end end,
            }
            "#,
        );
        let err = host.sort_key(0, &facts("a.jpg")).unwrap_err();
        assert_eq!(err.script, "test", "error must name the script");
        assert!(
            err.message.contains("too long"),
            "unexpected message: {}",
            err.message
        );
    }

    /// A runaway call must not poison the next one — the budget is per-call.
    #[test]
    fn budget_resets_between_calls() {
        let host = host_with(
            r#"
            vitrine.register_sort {
              name = "Spin",
              key = function(item) while true do end end,
            }
            vitrine.register_sort {
              name = "Fine",
              key = function(item) return item.name end,
            }
            "#,
        );
        assert!(host.sort_key(0, &facts("a.jpg")).is_err());
        assert_eq!(
            host.sort_key(1, &facts("a.jpg")).unwrap(),
            SortKey::Str("a.jpg".into())
        );
    }

    /// §16.7.2 limit 1: the dangerous libraries are absent, not merely hidden.
    #[test]
    fn dangerous_stdlib_is_not_loaded() {
        let host = ScriptHost::new().unwrap();
        for expr in ["os", "io", "package", "debug", "require", "load", "dofile"] {
            let err = host
                .load_str("probe", &format!("return {expr}.anything"))
                .unwrap_err();
            assert!(
                err.message.contains("nil"),
                "`{expr}` should be nil, got: {}",
                err.message
            );
        }
    }

    /// §16.7.2 limit 2: binary chunks are refused.
    #[test]
    fn binary_chunks_are_refused() {
        let host = ScriptHost::new().unwrap();
        // Lua bytecode starts with the ESC "Lua" signature.
        let err = host.load_str("evil", "\x1bLua\x54\x00").unwrap_err();
        assert_eq!(err.script, "evil");
        assert!(
            err.message.to_lowercase().contains("binary"),
            "unexpected message: {}",
            err.message
        );
    }

    /// A syntax error names its script and does not take the host down.
    #[test]
    fn syntax_error_is_attributed_and_survivable() {
        let host = ScriptHost::new().unwrap();
        let err = host.load_str("broken", "this is not lua").unwrap_err();
        assert_eq!(err.script, "broken");
        // The host still works afterwards.
        host.load_str(
            "ok",
            r#"vitrine.register_sort{name="X", key=function() return 1 end}"#,
        )
        .unwrap();
        assert_eq!(host.providers().len(), 1);
    }

    /// Reloading a script replaces its registrations rather than duplicating
    /// them — the property hot reload depends on.
    #[test]
    fn reload_replaces_rather_than_accumulates() {
        let host = ScriptHost::new().unwrap();
        let src = r#"vitrine.register_sort{name="One", key=function() return 1 end}"#;
        host.load_str("s", src).unwrap();
        host.load_str("s", src).unwrap();
        assert_eq!(host.providers().len(), 1);

        // A different script contributes its own, and is unaffected by s.
        host.load_str(
            "t",
            r#"vitrine.register_sort{name="Two", key=function() return 2 end}"#,
        )
        .unwrap();
        host.load_str("s", src).unwrap();
        assert_eq!(host.providers().len(), 2);
    }

    /// A key returning something uncomparable is an error, not a silent
    /// "everything sorts equal".
    #[test]
    fn non_scalar_key_is_an_error() {
        let host = host_with(r#"vitrine.register_sort{name="Bad", key=function() return {} end}"#);
        let err = host.sort_key(0, &facts("a.jpg")).unwrap_err();
        assert!(
            err.message.contains("expected a number or a string"),
            "unexpected: {}",
            err.message
        );
    }

    /// Mixed key types still produce a total, explicable order.
    #[test]
    fn mixed_key_types_have_a_total_order() {
        use std::cmp::Ordering;
        let n = SortKey::Num(1.0);
        let s = SortKey::Str("a".into());
        assert_eq!(n.cmp_key(&s), Ordering::Less);
        assert_eq!(s.cmp_key(&n), Ordering::Greater);
        assert_eq!(
            SortKey::Num(f64::NAN).cmp_key(&SortKey::Num(1.0)),
            Ordering::Greater
        );
    }

    /// The example we ship must actually work — otherwise the first thing a
    /// script author copies is broken. Loads the real file from `docs/`, so
    /// editing it without re-checking fails the gate.
    #[test]
    fn the_shipped_example_script_works() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/scripts/natural-sort.lua");
        let src = std::fs::read_to_string(path).expect("shipped example is missing");
        let host = ScriptHost::new().unwrap();
        host.load_str("natural-sort", &src)
            .expect("shipped example must load");

        let names: Vec<String> = host.providers().iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["Name (natural)", "Oldest first"]);

        // The order it exists to produce.
        let two = host.sort_key(0, &facts("IMG_2.jpg")).unwrap();
        let ten = host.sort_key(0, &facts("img_10.jpg")).unwrap();
        assert_eq!(two.cmp_key(&ten), std::cmp::Ordering::Less);

        // The gsub-returns-two-values trap the file warns about: a key must be
        // one value, and a string here rather than a number.
        assert!(matches!(two, SortKey::Str(_)));

        // `date_taken or mtime` must yield a number even when unenriched.
        let unenriched = ItemFacts {
            mtime: 1234,
            date_taken: None,
            ..Default::default()
        };
        assert_eq!(host.sort_key(1, &unenriched).unwrap(), SortKey::Num(1234.0));
    }

    /// The whole point of mlua's `send` feature: the host must be movable to a
    /// worker, because §16.2 forbids scripts anywhere near the main loop. This
    /// is a compile-time assertion — if the feature is ever dropped from
    /// Cargo.toml, this test fails to build rather than silently pinning the
    /// scripting tier to the main thread.
    #[test]
    fn host_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ScriptHost>();
    }

    /// Scripts see the documented API version.
    #[test]
    fn api_version_is_exposed() {
        let host = host_with(&format!(
            r#"
            assert(vitrine.api_version == {API_VERSION})
            vitrine.register_sort{{name="V", key=function() return 1 end}}
            "#
        ));
        assert_eq!(host.providers().len(), 1);
    }

    /// The facts table carries what §16.2 promises.
    #[test]
    fn item_facts_reach_the_script() {
        let host = host_with(
            r#"
            vitrine.register_sort {
              name = "Facts",
              key = function(item)
                return item.name .. "|" .. item.size .. "|" .. item.rating
                    .. "|" .. tostring(item.date_taken)
              end,
            }
            "#,
        );
        let f = ItemFacts {
            name: "a.jpg".into(),
            size: 42,
            rating: 3,
            date_taken: None,
            ..Default::default()
        };
        assert_eq!(
            host.sort_key(0, &f).unwrap(),
            SortKey::Str("a.jpg|42|3|nil".into())
        );
    }
}
