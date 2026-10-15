//! Writes the typed [`Config`] back into the `toml_edit` document.
//!
//! Tables are matched by identity (account label, job name or one of its
//! destination ids, destination id), so comments, key formatting and
//! unknown keys stay with the item they belong to. Only known keys are
//! written, and a value is replaced only when it actually changed.

use anyhow::{Context, Result};
use serde::Serialize;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use super::Config;

const ACCOUNT_KEYS: &[&str] = &["label", "remote", "drive_type", "added", "delta"];
const JOB_KEYS: &[&str] = &[
    "name",
    "source",
    "excludes",
    "copy_hidden",
    "source_volume_uuid",
    "source_volume_name",
    "source_mount_point",
    "source_volume_partition_uuid",
    "source_volume_device",
    "source_volume_root",
];
const DESTINATION_KEYS: &[&str] = &[
    "id",
    "type",
    "account",
    "path",
    "mode",
    "volume_uuid",
    "volume_name",
    "mount_point",
    "volume_partition_uuid",
    "volume_device",
    "volume_root",
];

/// Keys with a default: when the model holds the default, the key is only
/// written if the file already had it.
type Default = (&'static str, fn() -> Value);

const ACCOUNT_DEFAULTS: &[Default] = &[("delta", || Value::from(false))];
const JOB_DEFAULTS: &[Default] = &[
    ("excludes", || Value::Array(toml_edit::Array::new())),
    ("copy_hidden", || Value::from(false)),
];
const DESTINATION_DEFAULTS: &[Default] = &[("mode", || Value::from("one-way"))];

pub fn sync_document(doc: &mut DocumentMut, config: &Config) -> Result<()> {
    let root = doc.as_table_mut();
    set_value(root, "version", Value::from(config.version));

    let template = peek_tables(root, "accounts").into_iter().next();
    let mut pool = take_tables(root, "accounts");
    let mut accounts = ArrayOfTables::new();
    for account in &config.accounts {
        let found = claim(&mut pool, |t| str_key(t, "label") == Some(&account.label));
        let (mut table, indent) = existing_or_new(found, template.as_ref());
        merge(
            &mut table,
            &to_table(account)?,
            ACCOUNT_KEYS,
            ACCOUNT_DEFAULTS,
            indent.as_deref(),
        );
        accounts.push(table);
    }
    put_tables(root, "accounts", accounts);

    let job_template = peek_tables(root, "jobs").into_iter().next();
    let dest_template = peek_tables(root, "jobs")
        .iter()
        .flat_map(|j| peek_tables(j, "destinations"))
        .next();
    let mut pool = take_tables(root, "jobs");

    // First pass: exact name matches for every job, so that no job can take
    // another job's table through the id fallback below.
    let mut claimed: Vec<Option<Table>> = config
        .jobs
        .iter()
        .map(|job| claim(&mut pool, |t| str_key(t, "name") == Some(&job.name)))
        .collect();
    // Second pass: a renamed job is found again through its destination ids.
    for (job, slot) in config.jobs.iter().zip(claimed.iter_mut()) {
        if slot.is_some() {
            continue;
        }
        let ids: Vec<&str> = job.destinations.iter().map(|d| d.id()).collect();
        *slot = claim(&mut pool, |t| {
            peek_tables(t, "destinations")
                .iter()
                .any(|d| str_key(d, "id").is_some_and(|id| ids.contains(&id)))
        });
    }

    let mut jobs = ArrayOfTables::new();
    for (job, slot) in config.jobs.iter().zip(claimed) {
        let (mut table, indent) = existing_or_new(slot, job_template.as_ref());
        merge(
            &mut table,
            &to_table(job)?,
            JOB_KEYS,
            JOB_DEFAULTS,
            indent.as_deref(),
        );

        let sibling = peek_tables(&table, "destinations").into_iter().next();
        let template = sibling.as_ref().or(dest_template.as_ref());
        let mut dest_pool = take_tables(&mut table, "destinations");
        let mut destinations = ArrayOfTables::new();
        for dest in &job.destinations {
            let found = claim(&mut dest_pool, |t| str_key(t, "id") == Some(dest.id()));
            let (mut dt, indent) = existing_or_new(found, template);
            merge(
                &mut dt,
                &to_table(dest)?,
                DESTINATION_KEYS,
                DESTINATION_DEFAULTS,
                indent.as_deref(),
            );
            destinations.push(dt);
        }
        put_tables(&mut table, "destinations", destinations);
        jobs.push(table);
    }
    put_tables(root, "jobs", jobs);

    renumber(doc.as_table_mut(), &mut 0);
    Ok(())
}

/// Gives every table a fresh, increasing position in tree order. toml_edit
/// renders tables by position, so this makes the file follow the model's
/// order for accounts, jobs and destinations, while tables OneFolder does
/// not manage (such as `[extra]`) stay after the tables they followed.
fn renumber(table: &mut Table, next: &mut isize) {
    if !table.is_dotted() {
        table.set_position(Some(*next));
        *next += 1;
    }
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(t) => renumber(t, next),
            Item::ArrayOfTables(aot) => {
                for t in aot.iter_mut() {
                    renumber(t, next);
                }
            }
            _ => {}
        }
    }
}

/// The whitespace after the last line break of a decor prefix: the indentation.
fn indent_of(prefix: Option<&toml_edit::RawString>) -> String {
    let text = prefix.and_then(|r| r.as_str()).unwrap_or("");
    let tail = text.rsplit('\n').next().unwrap_or("");
    if tail.chars().all(|c| c == ' ' || c == '\t') {
        tail.to_string()
    } else {
        String::new()
    }
}

/// Indentation of the first key in `table`, if it has any keys.
fn key_indent(table: &Table) -> Option<String> {
    let (first, _) = table.iter().next()?;
    table.key(first).map(|k| indent_of(k.leaf_decor().prefix()))
}

/// The claimed table, or a new one indented like `template` so a new
/// `[[jobs.destinations]]` lines up with its neighbours. Also returns the
/// indentation to give keys that get added.
fn existing_or_new(found: Option<Table>, template: Option<&Table>) -> (Table, Option<String>) {
    if let Some(table) = found {
        let indent = key_indent(&table);
        return (table, indent);
    }
    let mut table = Table::new();
    let Some(t) = template else {
        return (table, None);
    };
    let indent = indent_of(t.decor().prefix());
    if !indent.is_empty() {
        table.decor_mut().set_prefix(format!("\n{indent}"));
    }
    (table, key_indent(t))
}

fn to_table<T: Serialize>(value: &T) -> Result<Table> {
    let doc = toml_edit::ser::to_document(value).context("could not serialise the config")?;
    Ok(doc.as_table().clone())
}

fn str_key<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
    table.get(key).and_then(Item::as_str)
}

/// Removes the first table in `pool` that matches.
fn claim(pool: &mut Vec<Table>, matches: impl Fn(&Table) -> bool) -> Option<Table> {
    let index = pool.iter().position(matches)?;
    Some(pool.remove(index))
}

/// The tables under `key`, from either `[[key]]` sections or an inline array of inline tables.
fn peek_tables(table: &Table, key: &str) -> Vec<Table> {
    match table.get(key) {
        Some(Item::ArrayOfTables(aot)) => aot.iter().cloned().collect(),
        Some(Item::Value(Value::Array(array))) => array
            .iter()
            .filter_map(Value::as_inline_table)
            .map(|t| t.clone().into_table())
            .collect(),
        _ => Vec::new(),
    }
}

/// Like [`peek_tables`] but takes the tables out, leaving the key in place
/// (as an empty item) so that its position in the parent is kept.
fn take_tables(table: &mut Table, key: &str) -> Vec<Table> {
    let tables = peek_tables(table, key);
    if let Some(item) = table.get_mut(key) {
        *item = Item::None;
    }
    tables
}

fn put_tables(table: &mut Table, key: &str, tables: ArrayOfTables) {
    if tables.is_empty() {
        table.remove(key);
    } else if let Some(item) = table.get_mut(key) {
        *item = Item::ArrayOfTables(tables);
    } else {
        table.insert(key, Item::ArrayOfTables(tables));
    }
}

/// Writes the known `keys` from `fresh` into `table`. Unchanged values keep
/// their formatting; changed values keep their surrounding comments; keys
/// missing from `fresh` are removed, unless they have a default and the
/// file had them, in which case the default is written; other keys are left
/// alone.
fn merge(
    table: &mut Table,
    fresh: &Table,
    keys: &[&str],
    defaults: &[Default],
    indent: Option<&str>,
) {
    for key in keys {
        match fresh.get(key).and_then(Item::as_value) {
            Some(value) => set_value_indented(table, key, value.clone(), indent),
            None => match defaults.iter().find(|(k, _)| k == key) {
                Some((_, default)) if table.contains_key(key) => {
                    set_value_indented(table, key, default(), indent)
                }
                _ => {
                    table.remove(key);
                }
            },
        }
    }
}

fn set_value(table: &mut Table, key: &str, value: Value) {
    set_value_indented(table, key, value, None)
}

fn set_value_indented(table: &mut Table, key: &str, mut value: Value, indent: Option<&str>) {
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            if !same(old, &value) {
                *value.decor_mut() = old.decor().clone();
                *old = value;
            }
        }
        Some(slot) => *slot = Item::Value(value),
        None => {
            value.decor_mut().clear();
            table.insert(key, Item::Value(value));
            if let (Some(indent), Some(mut k)) =
                (indent.filter(|i| !i.is_empty()), table.key_mut(key))
            {
                k.leaf_decor_mut().set_prefix(indent.to_string());
            }
        }
    }
}

/// Semantic equality, ignoring formatting and comments.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.value() == y.value(),
        (Value::Integer(x), Value::Integer(y)) => x.value() == y.value(),
        (Value::Float(x), Value::Float(y)) => x.value() == y.value(),
        (Value::Boolean(x), Value::Boolean(y)) => x.value() == y.value(),
        (Value::Datetime(x), Value::Datetime(y)) => x.value() == y.value(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| same(p, q))
        }
        (Value::InlineTable(x), Value::InlineTable(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => false,
    }
}
