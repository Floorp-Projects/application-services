/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
use crate::{FloorpWorkspacesError as Error, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn integer<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    use serde::de::Error;
    let value = Value::deserialize(d)?;
    let n = value
        .as_f64()
        .ok_or_else(|| D::Error::custom("expected number"))?;
    if !n.is_finite() || n < 0.0 || n > MAX_SAFE_INTEGER as f64 || n.fract() != 0.0 {
        return Err(D::Error::custom("expected non-negative JS safe integer"));
    }
    Ok(n as u64)
}

pub fn valid_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}
fn text(value: &str, max: usize) -> bool {
    value.encode_utf16().count() <= max
}
fn valid_url(value: &str) -> bool {
    text(value, 8192)
        && url::Url::parse(value).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none_or(str::is_empty)
                && url.host().is_some()
        })
}
// Display/copy a serialized WHATWG navigation target. The wire string remains
// intact in Entity for Desktop conflict comparisons, while Foundation receives
// an ordinary absolute HTTP(S) URL even for accepted inputs such as http:x.
fn navigation_url(value: &str) -> String {
    url::Url::parse(value)
        .expect("validated workspace URL")
        .to_string()
}

pub fn js_compare(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Clock {
    #[serde(deserialize_with = "integer")]
    pub modified_at: u64,
    pub modified_by: String,
}
impl Clock {
    fn valid(&self) -> bool {
        self.modified_at <= MAX_SAFE_INTEGER && valid_id(&self.modified_by)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Tab {
    pub id: String,
    pub url: String,
    pub title: String,
    pub pinned: bool,
    #[serde(deserialize_with = "integer")]
    pub position: u64,
    pub deleted: bool,
    #[serde(deserialize_with = "integer")]
    pub modified_at: u64,
    pub modified_by: String,
}
impl Tab {
    fn valid(&self) -> bool {
        valid_id(&self.id)
            && valid_url(&self.url)
            && text(&self.title, 1024)
            && self.position <= MAX_SAFE_INTEGER
            && !self.deleted
            && self.clock().valid()
    }
    fn clock(&self) -> Clock {
        Clock {
            modified_at: self.modified_at,
            modified_by: self.modified_by.clone(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Entity {
    Workspace {
        #[serde(deserialize_with = "integer")]
        version: u64,
        #[serde(rename = "workspaceId")]
        workspace_id: String,
        name: String,
        icon: Option<String>,
        metadata: Clock,
    },
    Tab {
        #[serde(deserialize_with = "integer")]
        version: u64,
        #[serde(rename = "workspaceId")]
        workspace_id: String,
        tab: Tab,
    },
}
impl Entity {
    fn valid(&self) -> bool {
        match self {
            Self::Workspace {
                version,
                workspace_id,
                name,
                icon,
                metadata,
            } => {
                *version == 1
                    && valid_id(workspace_id)
                    && text(name, 256)
                    && icon.as_ref().is_none_or(|v| text(v, 256))
                    && metadata.valid()
            }
            Self::Tab {
                version,
                workspace_id,
                tab,
            } => *version == 1 && valid_id(workspace_id) && tab.valid() && tab.id != *workspace_id,
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Workspace { workspace_id, .. } => workspace_id,
            Self::Tab { tab, .. } => &tab.id,
        }
    }
    fn workspace_id(&self) -> &str {
        match self {
            Self::Workspace { workspace_id, .. } | Self::Tab { workspace_id, .. } => workspace_id,
        }
    }
    fn clock(&self) -> &Clock {
        match self {
            Self::Workspace { metadata, .. } => metadata,
            _ => unreachable!(),
        }
    }
    // Desktop compares these deliberately ordered, normalized objects, not
    // the wire entity. Rust's serializer uses the same JSON string escaping
    // for well-formed Unicode. The final comparison is UTF-16, never UTF-8.
    fn tiebreak(&self) -> String {
        match self {
            Self::Workspace {
                name,
                icon,
                metadata,
                ..
            } => {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Metadata<'a> {
                    modified_at: u64,
                    modified_by: &'a str,
                    name: &'a str,
                    icon: &'a Option<String>,
                }
                serde_json::to_string(&Metadata {
                    modified_at: metadata.modified_at,
                    modified_by: &metadata.modified_by,
                    name,
                    icon,
                })
                .unwrap()
            }
            Self::Tab { tab, .. } => serde_json::to_string(tab).unwrap(),
        }
    }
    fn newer_than(&self, other: &Self) -> bool {
        let (a, b) = match (self, other) {
            (Self::Workspace { .. }, Self::Workspace { .. }) => {
                (self.clock().clone(), other.clock().clone())
            }
            (Self::Tab { tab: a, .. }, Self::Tab { tab: b, .. }) => (a.clock(), b.clock()),
            _ => return false,
        };
        a.modified_at
            .cmp(&b.modified_at)
            .then_with(|| js_compare(&a.modified_by, &b.modified_by))
            .then_with(|| js_compare(&self.tiebreak(), &other.tiebreak()))
            != Ordering::Less
    }
    // Field order is part of Desktop's normalized entity byte-size limit.
    fn normalized_json(&self) -> String {
        match self {
            Self::Workspace {
                version,
                workspace_id,
                name,
                icon,
                metadata,
            } => {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Wire<'a> {
                    version: u64,
                    kind: &'a str,
                    workspace_id: &'a str,
                    name: &'a str,
                    icon: &'a Option<String>,
                    metadata: &'a Clock,
                }
                serde_json::to_string(&Wire {
                    version: *version,
                    kind: "workspace",
                    workspace_id,
                    name,
                    icon,
                    metadata,
                })
                .unwrap()
            }
            Self::Tab {
                version,
                workspace_id,
                tab,
            } => {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Wire<'a> {
                    version: u64,
                    kind: &'a str,
                    workspace_id: &'a str,
                    tab: &'a Tab,
                }
                serde_json::to_string(&Wire {
                    version: *version,
                    kind: "tab",
                    workspace_id,
                    tab,
                })
                .unwrap()
            }
        }
    }
}

pub enum Record {
    Live(Entity),
    Deleted(String),
}
pub fn parse_record(id: &str, payload: &str) -> Result<Record> {
    if !valid_id(id) || payload.len() > 2 * 1024 * 1024 {
        return Err(Error::InvalidRecord);
    }
    let value: Value = serde_json::from_str(payload).map_err(|_| Error::InvalidRecord)?;
    if value.get("id").and_then(Value::as_str) != Some(id) {
        return Err(Error::InvalidRecord);
    }
    if value.get("deleted") == Some(&Value::Bool(true)) {
        return Ok(Record::Deleted(id.into()));
    }
    // The Desktop wire parser requires icon to be present, even if null.
    let raw = value.get("entity").ok_or(Error::InvalidRecord)?;
    if raw.get("kind").and_then(Value::as_str) == Some("workspace") && raw.get("icon").is_none() {
        return Err(Error::InvalidRecord);
    }
    let entity: Entity = serde_json::from_value(raw.clone()).map_err(|_| Error::InvalidRecord)?;
    if !entity.valid() || entity.id() != id || entity.normalized_json().len() > 16 * 1024 {
        return Err(Error::InvalidRecord);
    }
    Ok(Record::Live(entity))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    pub records: BTreeMap<String, Entity>,
    pub tombstones: BTreeSet<String>,
}
impl Cache {
    pub fn validate(&self) -> bool {
        self.records.iter().all(|(id, e)| {
            id == e.id()
                && e.valid()
                && e.normalized_json().len() <= 16 * 1024
                && !self.tombstones.contains(id)
                && !self.tombstones.contains(e.workspace_id())
                && !matches!(self.records.get(e.workspace_id()), Some(Entity::Tab { .. }))
        }) && self.tombstones.iter().all(|id| valid_id(id))
    }
    pub fn merge(&mut self, record: Record) -> Result<()> {
        match record {
            Record::Deleted(id) => {
                // Retire known child IDs as well. Removing their records must
                // not erase the proof that those IDs belonged to this parent.
                let children: Vec<String> = self
                    .records
                    .iter()
                    .filter(|(_, entity)| entity.workspace_id() == id)
                    .map(|(key, _)| key.clone())
                    .collect();
                self.tombstones.extend(children);
                self.records
                    .retain(|key, entity| key != &id && entity.workspace_id() != id);
                self.tombstones.insert(id);
            }
            Record::Live(entity) => {
                let id = entity.id().to_string();
                if self.tombstones.contains(&id) {
                    return Ok(());
                }
                if self.tombstones.contains(entity.workspace_id()) {
                    // A late child of a retired parent also cannot reappear
                    // later under a different parent with this same ID.
                    self.tombstones.insert(id);
                    return Ok(());
                }
                if let Some(old) = self.records.get(&id) {
                    if std::mem::discriminant(old) != std::mem::discriminant(&entity)
                        || old.workspace_id() != entity.workspace_id()
                    {
                        return Err(Error::InvalidRecord);
                    }
                    if !entity.newer_than(old) {
                        return Ok(());
                    }
                }
                // UUID namespaces are shared. Reject a metadata/tab collision,
                // including one where the parent has not arrived yet.
                if let Entity::Workspace { .. } = entity {
                    if self
                        .records
                        .values()
                        .any(|e| matches!(e,Entity::Tab{tab,..} if tab.id==id))
                    {
                        return Err(Error::InvalidRecord);
                    }
                } else if matches!(
                    self.records.get(entity.workspace_id()),
                    Some(Entity::Tab { .. })
                ) || self.records.values().any(|e| e.workspace_id() == id)
                {
                    return Err(Error::InvalidRecord);
                }
                self.records.insert(id, entity);
            }
        }
        Ok(())
    }
    pub fn workspaces(&self) -> Vec<WorkspacePreview> {
        self.records
            .values()
            .filter_map(|entity| {
                let Entity::Workspace {
                    workspace_id,
                    name,
                    icon,
                    ..
                } = entity
                else {
                    return None;
                };
                if self.tombstones.contains(workspace_id) {
                    return None;
                }
                let mut tabs: Vec<TabPreview> = self
                    .records
                    .values()
                    .filter_map(|e| match e {
                        Entity::Tab {
                            workspace_id: parent,
                            tab,
                            ..
                        } if parent == workspace_id && !self.tombstones.contains(&tab.id) => {
                            Some(TabPreview {
                                id: tab.id.clone(),
                                url: navigation_url(&tab.url),
                                title: tab.title.clone(),
                                pinned: tab.pinned,
                                position: tab.position,
                            })
                        }
                        _ => None,
                    })
                    .collect();
                // UUIDs have ASCII-only symbols. Case-insensitive base ordering
                // agrees with Desktop's localeCompare for its generated lowercase
                // UUIDs; explicit case fallback keeps mixed-case IDs deterministic.
                tabs.sort_by(|a, b| {
                    a.position
                        .cmp(&b.position)
                        .then_with(|| a.id.to_ascii_lowercase().cmp(&b.id.to_ascii_lowercase()))
                        .then_with(|| b.id.cmp(&a.id))
                });
                Some(WorkspacePreview {
                    id: workspace_id.clone(),
                    name: name.clone(),
                    icon: icon.clone(),
                    tabs,
                })
            })
            .collect()
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct WorkspacePreview {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub tabs: Vec<TabPreview>,
}
#[derive(Debug, Clone, Serialize)]
pub struct TabPreview {
    pub id: String,
    pub url: String,
    pub title: String,
    pub pinned: bool,
    pub position: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const W: &str = "AAAAAAAA-0000-0000-0000-000000000001";
    const T: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    const D: &str = "cccccccc-0000-0000-0000-000000000003";
    fn workspace(name: &str, time: u64) -> Record {
        parse_record(W,&json!({"id":W,"entity":{"version":1,"kind":"workspace","workspaceId":W,"name":name,"icon":null,"metadata":{"modifiedAt":time,"modifiedBy":D}}}).to_string()).unwrap()
    }
    fn tab(id: &str, parent: &str, title: &str) -> Record {
        parse_record(id,&json!({"id":id,"entity":{"version":1,"kind":"tab","workspaceId":parent,"tab":{"id":id,"url":"https://example.org/","title":title,"pinned":true,"position":0,"deleted":false,"modifiedAt":5,"modifiedBy":D}}}).to_string()).unwrap()
    }
    #[test]
    fn child_before_clock_zero_metadata_keeps_case_and_duplicate_urls() {
        let mut cache = Cache::default();
        cache.merge(tab(T, W, "one")).unwrap();
        assert!(cache.workspaces().is_empty());
        cache.merge(workspace("Desktop", 0)).unwrap();
        cache.merge(tab(D, W, "two")).unwrap();
        let previews = cache.workspaces();
        assert_eq!(previews[0].id, W);
        assert_eq!(previews[0].tabs.len(), 2);
        assert_eq!(previews[0].tabs[0].url, previews[0].tabs[1].url);
    }
    #[test]
    fn metadata_tie_break_uses_desktop_property_order_and_utf16_comparison() {
        let mut cache = Cache::default();
        cache.merge(workspace("\u{e000}", 5)).unwrap();
        cache.merge(workspace("\u{10000}", 5)).unwrap();
        assert_eq!(cache.workspaces()[0].name, "\u{e000}");
        cache.merge(workspace("newer", 6)).unwrap();
        assert_eq!(cache.workspaces()[0].name, "newer");
        assert_eq!(js_compare("\u{10000}", "\u{e000}"), Ordering::Less);
    }
    #[test]
    fn normalized_json_and_escaping_match_desktop() {
        let Record::Live(entity) = workspace("\n\t\"\\\u{2028}😀", 0) else {
            panic!()
        };
        assert_eq!(entity.tiebreak(),format!("{{\"modifiedAt\":0,\"modifiedBy\":\"{D}\",\"name\":\"\\n\\t\\\"\\\\\u{2028}😀\",\"icon\":null}}"));
        assert!(entity
            .normalized_json()
            .starts_with("{\"version\":1,\"kind\":\"workspace\",\"workspaceId\":"));
    }
    #[test]
    fn tombstones_dominate_old_new_and_late_children() {
        let mut cache = Cache::default();
        cache.merge(workspace("old", 0)).unwrap();
        cache.merge(tab(T, W, "tab")).unwrap();
        cache.merge(Record::Deleted(T.into())).unwrap();
        cache.merge(tab(T, W, "new")).unwrap();
        assert!(cache.workspaces()[0].tabs.is_empty());
        cache.merge(Record::Deleted(W.into())).unwrap();
        cache.merge(workspace("new", 100)).unwrap();
        cache.merge(tab(D, W, "late")).unwrap();
        assert!(cache.workspaces().is_empty());
        let restored: Cache =
            serde_json::from_str(&serde_json::to_string(&cache).unwrap()).unwrap();
        assert!(restored.tombstones.contains(W));
        assert!(restored.tombstones.contains(T));
        assert!(restored.tombstones.contains(D));
    }
    #[test]
    fn parent_deletion_preserves_retirement_of_known_children() {
        let mut cache = Cache::default();
        cache.merge(workspace("old", 0)).unwrap();
        cache.merge(tab(T, W, "child")).unwrap();
        cache.merge(Record::Deleted(W.into())).unwrap();
        assert!(cache.tombstones.contains(T));
        cache.merge(tab(T, D, "attempted move")).unwrap();
        assert!(!cache.records.contains_key(T));
    }

    #[test]
    fn ids_cannot_change_kind_or_parent() {
        let mut cache = Cache::default();
        cache.merge(workspace("old", 0)).unwrap();
        assert!(cache.merge(tab(W, D, "collision")).is_err());
        cache.merge(tab(T, W, "one")).unwrap();
        assert!(cache.merge(tab(T, D, "moved")).is_err());
    }
    #[test]
    fn desktop_uuid_grammar_and_utf16_boundaries() {
        assert!(valid_id(W));
        assert!(!valid_id("012345678901"));
        assert!(text(&"😀".repeat(128), 256));
        assert!(!text(&"😀".repeat(129), 256));
        assert!(valid_url(" HTTPS://example.org/a "));
        for url in [
            "about:blank",
            "file:///a",
            "https://user:pass@example.org/",
            "https://example.org:bad/",
            "https://",
        ] {
            assert!(!valid_url(url), "{url}");
        }
    }
    #[test]
    fn malformed_future_records_and_large_normalized_entities_fail() {
        let Record::Live(entity) = tab(T, W, "title") else {
            panic!()
        };
        let mut value = serde_json::to_value(entity).unwrap();
        value["version"] = json!(2);
        assert!(parse_record(T, &json!({"id":T,"entity":value}).to_string()).is_err());
        let mut value = match tab(T, W, "title") {
            Record::Live(e) => serde_json::to_value(e).unwrap(),
            _ => unreachable!(),
        };
        value["tab"]["modifiedAt"] = json!(-1);
        assert!(parse_record(T, &json!({"id":T,"entity":value}).to_string()).is_err());
        value["tab"]["modifiedAt"] = json!(9007199254740992_u64);
        assert!(parse_record(T, &json!({"id":T,"entity":value}).to_string()).is_err());
        value["tab"]["modifiedAt"] = json!(1.0);
        value["version"] = json!(1.0);
        assert!(parse_record(T, &json!({"id":T,"entity":value}).to_string()).is_ok());
        value["tab"]["url"] = json!(format!("https://example.org/{}", "中".repeat(8000)));
        assert!(parse_record(T, &json!({"id":T,"entity":value}).to_string()).is_err());
        assert!(parse_record(
            T,
            &format!(r#"{{"id":"{T}","entity":{{"kind":"tab","version":1,"title":"\ud800"}}}}"#)
        )
        .is_err());
    }
}

#[cfg(test)]
mod desktop_fixture_tests {
    use super::*;
    #[test]
    fn accepts_and_normalizes_the_pinned_desktop_fixture_exactly() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/desktop-v1.json")).unwrap();
        assert_eq!(
            fixture["desktopRevision"],
            "a218c5b9751dbfb6882721a51f916f15c5c5e99e"
        );
        for case in fixture["cases"].as_array().unwrap() {
            let result = parse_record(
                case["id"].as_str().unwrap(),
                case["payload"].as_str().unwrap(),
            );
            assert_eq!(
                result.is_ok(),
                case["accepted"].as_bool().unwrap(),
                "fixture {}",
                case["name"]
            );
            if let Ok(Record::Live(entity)) = result {
                if let Entity::Tab { tab, .. } = &entity {
                    assert_eq!(
                        navigation_url(&tab.url),
                        case["navigationUrl"].as_str().unwrap(),
                        "navigation fixture {}",
                        case["name"]
                    );
                }
                assert_eq!(
                    entity.normalized_json(),
                    case["normalized"].as_str().unwrap(),
                    "fixture {}",
                    case["name"]
                );
            }
        }
    }
}
