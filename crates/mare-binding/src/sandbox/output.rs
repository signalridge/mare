//! Exact per-run JSON output accounting, shared by VM output and public host helpers.
use mare_shared::lua::{json_to_lua_lossless, strict_lua_to_json};
use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use serde_json::Value as Json;

const LIMIT: &str = "sandbox output byte limit exceeded";
#[derive(Debug)]
pub(super) struct Ledger {
    max: usize,
    bytes: usize,
    items: Vec<Json>,
    limited: bool,
}
pub(super) fn json_size<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Result<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}
pub(super) fn size(value: &Json) -> usize {
    json_size(value).expect("JSON value serialization")
}
fn prefix(text: &str, available: usize) -> String {
    if available < 2 {
        return String::new();
    }
    let mut used = 2;
    let mut end = 0;
    for (offset, ch) in text.char_indices() {
        let cost = match ch {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            ch if ch < '\u{20}' => 6,
            ch => ch.len_utf8(),
        };
        if used + cost > available {
            break;
        }
        used += cost;
        end = offset + ch.len_utf8();
    }
    text[..end].to_owned()
}
fn entry_prefix(item: &Json, available: usize) -> Option<Json> {
    if let Some(text) = item.as_str() {
        let text = prefix(text, available);
        return (!text.is_empty()).then_some(Json::String(text));
    }
    if item.get("type").and_then(Json::as_str) == Some("text") {
        let mut block = item.clone();
        let text = block.get("text")?.as_str()?.to_owned();
        block["text"] = Json::String(String::new());
        let overhead = size(&block).saturating_sub(2);
        let text = prefix(&text, available.saturating_sub(overhead));
        if !text.is_empty() {
            block["text"] = Json::String(text);
            return (size(&block) <= available).then_some(block);
        }
    }
    None
}
pub(super) struct Finished {
    pub items: Vec<Json>,
    pub value: Option<Json>,
    pub error: Option<String>,
    pub limited: bool,
    pub bytes: usize,
}
impl Ledger {
    pub fn new(max: usize) -> Self {
        Self { max, bytes: 2, items: vec![], limited: false }
    }
    pub fn admit(&mut self, item: Json) -> bool {
        if self.limited {
            return false;
        }
        let next = self.bytes.saturating_add(size(&item)).saturating_add(usize::from(!self.items.is_empty()));
        if next > self.max {
            self.limited = true;
            return false;
        }
        self.bytes = next;
        self.items.push(item);
        true
    }
    pub fn finish(&self, value: Option<Json>, error: Option<String>) -> Finished {
        let extra =
            error.as_ref().map(|e| size(&Json::String(e.clone()))).or_else(|| value.as_ref().map(size)).unwrap_or(0);
        if !self.limited && self.bytes.saturating_add(extra) <= self.max {
            return Finished {
                items: self.items.clone(),
                value: if error.is_none() { value } else { None },
                error,
                limited: false,
                bytes: self.bytes + extra,
            };
        }
        let diagnostic = prefix(LIMIT, self.max.saturating_sub(2));
        let diagnostic_bytes = size(&Json::String(diagnostic.clone()));
        let budget = self.max.saturating_sub(diagnostic_bytes);
        let mut bytes: usize = 2;
        let mut items = vec![];
        for item in &self.items {
            let comma = usize::from(!items.is_empty());
            let available = budget.saturating_sub(bytes.saturating_add(comma));
            if size(item) <= available {
                bytes += size(item) + comma;
                items.push(item.clone());
            } else {
                if let Some(item) = entry_prefix(item, available) {
                    bytes += size(&item) + comma;
                    items.push(item);
                }
                break;
            }
        }
        Finished { items, value: None, error: Some(diagnostic), limited: true, bytes: bytes + diagnostic_bytes }
    }
}
impl UserData for Ledger {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("admit", |lua, this, value: Value| Ok(this.admit(strict_lua_to_json(lua, value)?)));
        methods.add_method("finish", |lua, this, (value, error): (Value, Option<String>)| {
            let value = if matches!(value, Value::Nil) { None } else { Some(strict_lua_to_json(lua, value)?) };
            let result = this.finish(value, error);
            let out = lua.create_table()?;
            out.set("items", json_to_lua_lossless(lua, &Json::Array(result.items))?)?;
            if let Some(value) = result.value {
                out.set("result", json_to_lua_lossless(lua, &value)?)?;
            }
            out.set("error", result.error)?;
            out.set("limited", result.limited)?;
            out.set("bytes", result.bytes)?;
            Ok(out)
        });
    }
}
pub(super) fn install(lua: &Lua, sandbox: &Table) -> mlua::Result<()> {
    sandbox.set(
        "output_ledger",
        lua.create_function(|_, max: usize| {
            if !(4..=16 * 1024 * 1024).contains(&max) {
                return Err(mlua::Error::runtime("output ledger max_bytes must be 4..16777216"));
            }
            Ok(Ledger::new(max))
        })?,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_array_json_and_completion_boundary() {
        let items = vec![serde_json::json!("\"\n\u{1}雪"), serde_json::json!({"type":"image","data":"YQ=="})];
        let value = serde_json::json!(["\\", 1]);
        let budget = size(&Json::Array(items.clone())) + size(&value);
        let mut ledger = Ledger::new(budget);
        for item in items.clone() {
            assert!(ledger.admit(item));
        }
        let result = ledger.finish(Some(value.clone()), None);
        assert!(!result.limited);
        assert_eq!(result.bytes, budget);
        let result = ledger.finish(Some(serde_json::json!(["\\", 10])), None);
        assert!(result.limited);
        assert!(result.bytes <= budget);
        assert_eq!(size(&Json::Array(result.items)) + size(&Json::String(result.error.unwrap())), result.bytes);
    }
    #[test]
    fn limited_prefix_and_diagnostic_always_fit_even_tiny_caps() {
        for max in 4..160 {
            let mut ledger = Ledger::new(max);
            ledger.admit(serde_json::json!({"type":"text","text":"雪\n\"\\".repeat(30)}));
            let result = ledger.finish(None, Some("diagnostic".repeat(40)));
            assert!(result.limited);
            assert!(result.bytes <= max);
            assert_eq!(size(&Json::Array(result.items)) + size(&Json::String(result.error.unwrap())), result.bytes);
        }
    }
}

#[cfg(test)]
mod public_tests {
    #[test]
    fn public_ledger_copies_entries_and_counts_json_not_lua_bytes() {
        let lua = mlua::Lua::new();
        let api = lua.create_table().unwrap();
        super::install(&lua, &api).unwrap();
        lua.globals().set("sandbox", api).unwrap();
        lua.load(
            r#"
            assert(not pcall(sandbox.output_ledger,3))
            local l=sandbox.output_ledger(100)
            local entry={type='text',text='snow 雪\n"'}
            assert(l:admit(entry));entry.text='changed'
            local r=l:finish(nil,'error')
            assert(not r.limited and r.items[1].text=='snow 雪\n"')
            r.items[1].text='changed again'
            assert(l:finish().items[1].text=='snow 雪\n"')
            local cycle={};cycle.self=cycle
            assert(not pcall(l.admit,l,cycle))
            local tiny=sandbox.output_ledger(4)
            assert(not tiny:admit('x'))
            assert(not tiny:admit(''))
            local r=tiny:finish()
            assert(r.limited and r.bytes==4 and #r.items==0 and r.error=='')
        "#,
        )
        .exec()
        .unwrap();
    }
}
