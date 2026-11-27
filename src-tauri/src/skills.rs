use crate::plugins::InstalledPluginRecord;
use crate::security::resolve_jailed_path;
use crate::workspace::clean_path_string;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

const MAX_DISCOVERED_SKILLS: usize = 100;
const MAX_SKILL_MD_BYTES: u64 = 256 * 1024;
const MAX_RESOURCE_SCAN_BYTES: u64 = 512 * 1024;
const MAX_RESOURCE_READ_BYTES: u64 = 128 * 1024;

const MAX_SKILL_RESOURCE_FILES: usize = 160;
const SKILL_RESOURCE_SUBDIRS: &[&str] = &[
    "references",
    "scripts",
    "assets",
    "examples",
    "resources",
    "guides",
    "docs",
];
const PLUGIN_SHARED_RESOURCE_SUBDIRS: &[&str] = &["agents", "resources", "docs", "policies"];
const PLUGIN_SKILL_CONTAINERS: &[&str] = &["skills", ".claude/skills", ".gemini/skills"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Workspace,
    Plugin,
    Global,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub scope: SkillScope,
    pub source_label: String,
    pub plugin_id: Option<String>,
    pub enabled: bool,
    pub disable_model_invocation: bool,
    pub dir_path: String,
    pub resource_files: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub catalog_tokens_est: usize,
    pub body_tokens_est: usize,
    pub parse_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub summary: SkillSummary,
    pub canon_dir: PathBuf,
    pub plugin_root: Option<PathBuf>,
    pub body_md: String,
    pub raw_md: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillDetail {
    pub summary: SkillSummary,
    pub body_md: String,
    pub raw_md: String,
}

#[derive(Debug, Clone)]
pub struct ParsedSkill {
    pub name: String,
    pub description: String,
    #[allow(dead_code)]
    pub license: Option<String>,
    #[allow(dead_code)]
    pub compatibility: Option<String>,
    pub allowed_tools: Vec<String>,
    pub disable_model_invocation: bool,
    #[allow(dead_code)]
    pub metadata: BTreeMap<String, String>,
    pub body_md: String,
}

pub fn is_valid_skill_name(name: &str) -> bool {
    let len = name.len();
    if len == 0 || len > 64 {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn sanitize_skill_name(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            out.push(ch);
            prev_dash = false;
        } else if (ch == '-' || ch == '_' || ch == ' ') && !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 64 {
        out.truncate(64);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

fn unquote_scalar(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(ch);
            }
        }
        return out;
    }
    if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        return trimmed[1..trimmed.len() - 1].replace("''", "'");
    }
    trimmed.to_string()
}

fn parse_allowed_tools_inline(val: &str) -> Vec<String> {
    let trimmed = val.trim();
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    inner
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(unquote_scalar)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_block_scalar(lines: &[&str], start_idx: usize, indicator: &str) -> (String, usize) {
    let folded = indicator.starts_with('>');
    let chomp = indicator.chars().nth(1);
    let mut raw_block: Vec<&str> = Vec::new();
    let mut i = start_idx;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            raw_block.push("");
            i += 1;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            raw_block.push(line);
            i += 1;
        } else {
            break;
        }
    }

    while matches!(raw_block.last(), Some(&"")) && chomp != Some('+') {
        raw_block.pop();
    }

    let min_indent = raw_block
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);

    let dedented: Vec<&str> = raw_block
        .into_iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                let mut byte_off = 0;
                for (cnt, (idx, _)) in l.char_indices().enumerate() {
                    if cnt == min_indent {
                        byte_off = idx;
                        break;
                    }
                    byte_off = l.len();
                }
                &l[byte_off..]
            }
        })
        .collect();

    let text = if folded {
        let mut paragraphs: Vec<String> = Vec::new();
        let mut cur: Vec<&str> = Vec::new();
        for l in dedented {
            if l.is_empty() {
                if !cur.is_empty() {
                    paragraphs.push(cur.join(" "));
                    cur.clear();
                }
            } else {
                cur.push(l.trim());
            }
        }
        if !cur.is_empty() {
            paragraphs.push(cur.join(" "));
        }
        paragraphs.join("\n")
    } else {
        dedented.join("\n")
    };

    (text.trim_end().to_string(), i)
}

pub fn parse_skill_markdown(
    raw: &str,
    fallback_dir_name: Option<&str>,
) -> Result<ParsedSkill, String> {
    let cleaned = raw
        .strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<&str> = cleaned.lines().collect();

    if lines.first().map(|l| l.trim()) != Some("---") {
        return Err("SKILL.md wajib dimulai dengan frontmatter YAML ('---').".to_string());
    }

    let Some(end_idx) = lines.iter().enumerate().skip(1).find_map(|(idx, l)| {
        if l.trim() == "---" {
            Some(idx)
        } else {
            None
        }
    }) else {
        return Err("Frontmatter YAML SKILL.md tidak memiliki penutup '---'.".to_string());
    };

    let fm_lines = &lines[1..end_idx];
    let mut raw_name: Option<String> = None;
    let mut description = String::new();
    let mut license: Option<String> = None;
    let mut compatibility: Option<String> = None;
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut disable_model_invocation = false;
    let mut metadata: BTreeMap<String, String> = BTreeMap::new();

    let mut i = 0;
    while i < fm_lines.len() {
        let line = fm_lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            i += 1;
            continue;
        }
        let Some((k_raw, v_raw)) = line.split_once(':') else {
            i += 1;
            continue;
        };
        let key = k_raw.trim().to_ascii_lowercase();
        let val_trim = v_raw.trim();

        if matches!(val_trim, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            let (block_val, next_i) = parse_block_scalar(fm_lines, i + 1, val_trim);
            match key.as_str() {
                "description" => description = block_val,
                "name" => raw_name = Some(block_val.trim().to_string()),
                "license" => {
                    if !block_val.is_empty() {
                        license = Some(block_val);
                    }
                }
                "compatibility" => {
                    if !block_val.is_empty() {
                        compatibility = Some(block_val);
                    }
                }
                _ => {}
            }
            i = next_i;
            continue;
        }

        if key == "metadata" && val_trim.is_empty() {
            let mut j = i + 1;
            while j < fm_lines.len() {
                let sub = fm_lines[j];
                if sub.trim().is_empty() {
                    j += 1;
                    continue;
                }
                if !(sub.starts_with(' ') || sub.starts_with('\t')) {
                    break;
                }
                if let Some((mk, mv)) = sub.trim().split_once(':') {
                    metadata.insert(mk.trim().to_string(), unquote_scalar(mv));
                }
                j += 1;
            }
            i = j;
            continue;
        }

        if (key == "allowed-tools" || key == "allowed_tools") && val_trim.is_empty() {
            let mut j = i + 1;
            while j < fm_lines.len() {
                let sub = fm_lines[j];
                if sub.trim().is_empty() {
                    j += 1;
                    continue;
                }
                if !(sub.starts_with(' ') || sub.starts_with('\t')) {
                    break;
                }
                if let Some(item) = sub.trim().strip_prefix('-') {
                    let clean_item = unquote_scalar(item);
                    if !clean_item.is_empty() {
                        allowed_tools.push(clean_item);
                    }
                }
                j += 1;
            }
            i = j;
            continue;
        }

        let scalar = unquote_scalar(val_trim);
        match key.as_str() {
            "name" => raw_name = Some(scalar),
            "description" => description = scalar,
            "license" => {
                if !scalar.is_empty() {
                    license = Some(scalar);
                }
            }
            "compatibility" => {
                if !scalar.is_empty() {
                    compatibility = Some(scalar);
                }
            }
            "allowed-tools" | "allowed_tools" => {
                allowed_tools = parse_allowed_tools_inline(val_trim);
            }
            "disable-model-invocation" | "disable_model_invocation" => {
                disable_model_invocation =
                    matches!(scalar.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1");
            }
            _ => {}
        }
        i += 1;
    }

    let description_clean = description.trim().to_string();
    if description_clean.is_empty() {
        return Err("Frontmatter SKILL.md wajib memiliki 'description' non-kosong.".to_string());
    }

    let name = match raw_name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(explicit) => {
            if !is_valid_skill_name(&explicit) {
                return Err(format!(
                    "Nama skill '{explicit}' tidak valid (wajib 1-64 char, [a-z0-9-], tanpa '--' atau '-' di ujung)."
                ));
            }
            explicit
        }
        None => {
            let fallback = fallback_dir_name
                .map(sanitize_skill_name)
                .unwrap_or_default();
            if !is_valid_skill_name(&fallback) {
                return Err("Nama skill kosong dan fallback nama folder tidak valid.".to_string());
            }
            fallback
        }
    };

    let body_md = if end_idx + 1 < lines.len() {
        lines[end_idx + 1..]
            .join("\n")
            .trim_start_matches('\n')
            .to_string()
    } else {
        String::new()
    };

    Ok(ParsedSkill {
        name,
        description: description_clean,
        license,
        compatibility,
        allowed_tools,
        disable_model_invocation,
        metadata,
        body_md,
    })
}

fn scan_resource_dir_recursive(
    canon_root: &Path,
    dir: &Path,
    depth: usize,
    prefix: Option<&str>,
    out: &mut Vec<String>,
) {
    if depth > 4 || out.len() >= MAX_SKILL_RESOURCE_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for path in children {
        if out.len() >= MAX_SKILL_RESOURCE_FILES {
            break;
        }
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if fname.starts_with('.') {
            continue;
        }
        let Ok(canon_entry) = fs::canonicalize(&path) else {
            continue;
        };
        if !canon_entry.starts_with(canon_root) {
            continue;
        }
        let Ok(meta) = fs::metadata(&canon_entry) else {
            continue;
        };
        if meta.is_dir() {
            scan_resource_dir_recursive(canon_root, &canon_entry, depth + 1, prefix, out);
        } else if meta.is_file() && meta.len() <= MAX_RESOURCE_SCAN_BYTES {
            if let Ok(rel) = canon_entry.strip_prefix(canon_root) {
                let rel_slash = rel.to_string_lossy().replace('\\', "/");
                if !rel_slash.is_empty() {
                    match prefix {
                        Some(pfx) => out.push(format!("{pfx}/{rel_slash}")),
                        None => out.push(rel_slash),
                    }
                }
            }
        }
    }
}

fn scan_subdirs_into(
    canon_root: &Path,
    subdirs: &[&str],
    prefix: Option<&str>,
    out: &mut Vec<String>,
) {
    for sub in subdirs {
        let sub_path = canon_root.join(sub);
        if let Ok(canon_sub) = fs::canonicalize(&sub_path) {
            if canon_sub.starts_with(canon_root) && canon_sub.is_dir() {
                scan_resource_dir_recursive(canon_root, &canon_sub, 1, prefix, out);
            }
        }
    }
}

fn scan_skill_resources(canon_skill_dir: &Path, canon_plugin_root: Option<&Path>) -> Vec<String> {
    let mut files = Vec::new();
    scan_subdirs_into(canon_skill_dir, SKILL_RESOURCE_SUBDIRS, None, &mut files);
    if let Some(plg_root) = canon_plugin_root {
        scan_subdirs_into(plg_root, PLUGIN_SHARED_RESOURCE_SUBDIRS, Some("plugin"), &mut files);
    }
    files.sort();
    files.dedup();
    files
}

fn load_single_skill_dir(
    skill_dir: &Path,
    jail_root: Option<&Path>,
    scope: SkillScope,
    source_label: &str,
    plugin_id: Option<&str>,
    disabled_set: &HashSet<String>,
) -> Option<SkillEntry> {
    let skill_md_path = skill_dir.join("SKILL.md");
    if !skill_md_path.is_file() {
        return None;
    }

    let Ok(canon_dir) = fs::canonicalize(skill_dir) else {
        return None;
    };
    let canon_root_opt = jail_root.map(|r| fs::canonicalize(r).unwrap_or_else(|_| r.to_path_buf()));
    if let Some(ref canon_root) = canon_root_opt {
        if !canon_dir.starts_with(canon_root) {
            return None;
        }
    }
    let plugin_root = if matches!(scope, SkillScope::Plugin) {
        canon_root_opt
    } else {
        None
    };
    let Ok(canon_md) = fs::canonicalize(&skill_md_path) else {
        return None;
    };
    if !canon_md.starts_with(&canon_dir) {
        return None;
    }

    let dir_name = skill_dir
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let clean_dir = clean_path_string(&canon_dir);

    let meta = match fs::metadata(&canon_md) {
        Ok(m) => m,
        Err(e) => {
            let fallback_name = dir_name
                .as_deref()
                .map(sanitize_skill_name)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "invalid-skill".to_string());
            return Some(SkillEntry {
                summary: SkillSummary {
                    name: fallback_name,
                    description: String::new(),
                    scope,
                    source_label: source_label.to_string(),
                    plugin_id: plugin_id.map(|s| s.to_string()),
                    enabled: false,
                    disable_model_invocation: false,
                    dir_path: clean_dir,
                    resource_files: Vec::new(),
                    allowed_tools: Vec::new(),
                    catalog_tokens_est: 0,
                    body_tokens_est: 0,
                    parse_error: Some(format!("Gagal membaca metadata SKILL.md: {e}")),
                },
                canon_dir,
                plugin_root,
                body_md: String::new(),
                raw_md: String::new(),
            });
        }
    };

    if meta.len() > MAX_SKILL_MD_BYTES {
        let fallback_name = dir_name
            .as_deref()
            .map(sanitize_skill_name)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "oversized-skill".to_string());
        return Some(SkillEntry {
            summary: SkillSummary {
                name: fallback_name,
                description: String::new(),
                scope,
                source_label: source_label.to_string(),
                plugin_id: plugin_id.map(|s| s.to_string()),
                enabled: false,
                disable_model_invocation: false,
                dir_path: clean_dir,
                resource_files: Vec::new(),
                allowed_tools: Vec::new(),
                catalog_tokens_est: 0,
                body_tokens_est: 0,
                parse_error: Some("Ukuran SKILL.md melebihi batas maksimum 256 KB.".to_string()),
            },
            canon_dir,
            plugin_root,
            body_md: String::new(),
            raw_md: String::new(),
        });
    }

    let raw_md = match fs::read_to_string(&canon_md) {
        Ok(s) => s,
        Err(e) => {
            let fallback_name = dir_name
                .as_deref()
                .map(sanitize_skill_name)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unreadable-skill".to_string());
            return Some(SkillEntry {
                summary: SkillSummary {
                    name: fallback_name,
                    description: String::new(),
                    scope,
                    source_label: source_label.to_string(),
                    plugin_id: plugin_id.map(|s| s.to_string()),
                    enabled: false,
                    disable_model_invocation: false,
                    dir_path: clean_dir,
                    resource_files: Vec::new(),
                    allowed_tools: Vec::new(),
                    catalog_tokens_est: 0,
                    body_tokens_est: 0,
                    parse_error: Some(format!("Gagal membaca SKILL.md sebagai UTF-8: {e}")),
                },
                canon_dir,
                plugin_root,
                body_md: String::new(),
                raw_md: String::new(),
            });
        }
    };

    let resource_files = scan_skill_resources(&canon_dir, plugin_root.as_deref());
    match parse_skill_markdown(&raw_md, dir_name.as_deref()) {
        Ok(parsed) => {
            let name_lower = parsed.name.to_ascii_lowercase();
            let enabled = !disabled_set.contains(&name_lower);
            let catalog_tokens_est = (parsed.name.len() + parsed.description.len()).div_ceil(4);
            let body_tokens_est = parsed.body_md.len().div_ceil(4);
            Some(SkillEntry {
                summary: SkillSummary {
                    name: parsed.name,
                    description: parsed.description,
                    scope,
                    source_label: source_label.to_string(),
                    plugin_id: plugin_id.map(|s| s.to_string()),
                    enabled,
                    disable_model_invocation: parsed.disable_model_invocation,
                    dir_path: clean_dir,
                    resource_files,
                    allowed_tools: parsed.allowed_tools,
                    catalog_tokens_est,
                    body_tokens_est,
                    parse_error: None,
                },
                canon_dir,
                plugin_root,
                body_md: parsed.body_md,
                raw_md,
            })
        }
        Err(err) => {
            let fallback_name = dir_name
                .as_deref()
                .map(sanitize_skill_name)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "invalid-skill".to_string());
            Some(SkillEntry {
                summary: SkillSummary {
                    name: fallback_name,
                    description: String::new(),
                    scope,
                    source_label: source_label.to_string(),
                    plugin_id: plugin_id.map(|s| s.to_string()),
                    enabled: false,
                    disable_model_invocation: false,
                    dir_path: clean_dir,
                    resource_files,
                    allowed_tools: Vec::new(),
                    catalog_tokens_est: 0,
                    body_tokens_est: 0,
                    parse_error: Some(err),
                },
                canon_dir,
                plugin_root,
                body_md: String::new(),
                raw_md,
            })
        }
    }
}

fn scan_skills_container(
    container_dir: &Path,
    jail_root: Option<&Path>,
    scope: SkillScope,
    source_label: &str,
    plugin_id: Option<&str>,
    disabled_set: &HashSet<String>,
    valid_map: &mut BTreeMap<String, SkillEntry>,
    broken_list: &mut Vec<SkillEntry>,
) {
    if !container_dir.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(container_dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();

    for subdir in subdirs {
        if valid_map.len() + broken_list.len() >= MAX_DISCOVERED_SKILLS {
            break;
        }
        let Some(entry) = load_single_skill_dir(
            &subdir,
            jail_root,
            scope.clone(),
            source_label,
            plugin_id,
            disabled_set,
        ) else {
            continue;
        };
        if entry.summary.parse_error.is_some() {
            broken_list.push(entry);
        } else {
            let key = entry.summary.name.to_ascii_lowercase();
            valid_map.entry(key).or_insert(entry);
        }
    }
}

fn scan_plugin_skill_containers(
    plugin_root: &Path,
    source_label: &str,
    plugin_id: Option<&str>,
    disabled_set: &HashSet<String>,
    valid_map: &mut BTreeMap<String, SkillEntry>,
    broken_list: &mut Vec<SkillEntry>,
) {
    for rel in PLUGIN_SKILL_CONTAINERS {
        if valid_map.len() + broken_list.len() >= MAX_DISCOVERED_SKILLS {
            break;
        }
        let container = plugin_root.join(rel);
        scan_skills_container(
            &container,
            Some(plugin_root),
            SkillScope::Plugin,
            source_label,
            plugin_id,
            disabled_set,
            valid_map,
            broken_list,
        );
    }
}

pub fn discover_skills(
    workspace_roots: &[(String, PathBuf)],
    plugins: &[InstalledPluginRecord],
    global_skills_dir: Option<&Path>,
    disabled_skills: &HashSet<String>,
) -> Vec<SkillEntry> {
    let disabled_norm: HashSet<String> = disabled_skills
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    let mut valid_map: BTreeMap<String, SkillEntry> = BTreeMap::new();
    let mut broken_list: Vec<SkillEntry> = Vec::new();

    // Tier 1: Workspace skills (highest priority)
    for (alias, root) in workspace_roots {
        let src_label = format!("workspace:{alias}");
        for rel in [".airlock/skills", ".agents/skills", ".claude/skills"] {
            if valid_map.len() + broken_list.len() >= MAX_DISCOVERED_SKILLS {
                break;
            }
            let container = root.join(rel);
            scan_skills_container(
                &container,
                Some(root),
                SkillScope::Workspace,
                &src_label,
                None,
                &disabled_norm,
                &mut valid_map,
                &mut broken_list,
            );
        }
    }

    // Tier 2: Enabled & trusted plugins (deterministic name order)
    let mut active_plugins: Vec<&InstalledPluginRecord> = plugins
        .iter()
        .filter(|p| p.enabled && p.trusted)
        .collect();
    active_plugins.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    for plugin in active_plugins {
        if valid_map.len() + broken_list.len() >= MAX_DISCOVERED_SKILLS {
            break;
        }
        let plg_dir = PathBuf::from(&plugin.install_dir);
        let src_label = format!("plugin:{}", plugin.name);
        scan_plugin_skill_containers(
            &plg_dir,
            &src_label,
            Some(&plugin.id),
            &disabled_norm,
            &mut valid_map,
            &mut broken_list,
        );
    }

    // Tier 3: Global skills
    if let Some(gdir) = global_skills_dir {
        if valid_map.len() + broken_list.len() < MAX_DISCOVERED_SKILLS {
            scan_skills_container(
                gdir,
                Some(gdir),
                SkillScope::Global,
                "global",
                None,
                &disabled_norm,
                &mut valid_map,
                &mut broken_list,
            );
        }
    }

    let mut combined: Vec<SkillEntry> = valid_map.into_values().collect();
    combined.extend(broken_list);
    combined
}

pub const BUILTIN_WORKFLOW_SKILL_NAME: &str = "airlock-blueprint-workflow";

const BUILTIN_WORKFLOW_SKILL_DESC: &str = "Structured multi-turn WebMCP & Antigravity workflow: (1) multi-turn inspect with relevant skills, (2) max 1 blueprint per turn in .gemini/blueprints/, (3) save stage-aware checkpoint with dual WebMCP + Antigravity handoff prompts, (4) self-pacing implementation (pre-flight checkup, 1 phase/turn) ending every active turn with 1 follow-up question and 2-4 concise choices.";

const BUILTIN_WORKFLOW_SKILL_BODY: &str = r#"# Airlock Multi-Turn Blueprint & Handoff Workflow

Follow this 4-stage discipline strictly in WebMCP sessions so context stays sharp and handoffs never collapse into monolithic 1-turn dumps.

## Turn-End Follow-Up Choices Protocol (MANDATORY on EVERY active turn)
- At the end of EVERY turn during `inspect`, `blueprint`, `handoff`, or `implement`, ALWAYS close with:
  1. **1 short follow-up question** naming what comes next.
  2. **2-4 concise, single-line actionable choices** the user can click or reply with immediately.
- Never silently finish without offering next-step choices when work spans multiple turns or awaits direction.

## Stage 1 — `inspect` (Multi-Turn Exploration)
1. Call `list_skills` first and `load_skill(name)` for every domain/architecture/debugging skill matching the task.
2. Explore using parallel `batch_inspect` calls (or `delegate_local_agent(mode="read_only")` for broad sweeps).
3. If the target surface is large, split inspection cleanly across turns by subsystem. Do NOT modify product code or write blueprints yet.
4. Each turn: report verified files/symbols, list remaining areas (if any), and close with 2-4 choices (e.g. inspect next area / proceed to blueprint list / save checkpoint & handoff prompts).

## Stage 2 — `blueprint` (Max 1 Blueprint per Turn)
1. Call `list_skills` + `load_skill` for specification/architecture/UI/domain skills matching the feature.
2. Agree on the blueprint file list first when creating multiple blueprints.
3. Write **AT MOST ONE complete blueprint markdown file per turn** under `.gemini/blueprints/<NN>-<slug>.md` (or the workspace's existing `.gemini/Blueprints/` folder).
4. **STRICT:** Never mix implementation code edits into a blueprint turn, and never generate multiple blueprint files in one turn.
5. After finishing 1 blueprint, STOP and offer choices (draft next blueprint / revise this blueprint / save checkpoint & generate handoff prompts).

## Stage 3 — `handoff` (Checkpoint + Dual Next-Session Prompts)
1. Call `session_checkpoint(op="save", stage="inspect"|"blueprint"|"implement", goal=..., blueprint_path=..., blueprint_paths=[...], recommended_skills=[...], decisions=[...], completed=[...], next_steps=[...])`.
2. Present **BOTH** `WEBMCP_NEXT_SESSION_PROMPT` and `ANTIGRAVITY_HANDOFF_PROMPT` from the tool output **verbatim** (never summarize or merge them into one monolithic prompt).
3. Close with follow-up choices letting the user pick WebMCP continuation, Antigravity handoff, or edits.

## Stage 4 — `implement` (Self-Pacing Multi-Turn for 1 Blueprint)
1. Load `.airlock/HANDOFF.md`, the checkpoint, and the single target blueprint.
2. Call `list_skills` + `load_skill` for the recommended implementation/TDD/verification skills.
3. **PRE-FLIGHT CHECKUP (Before First Edit):** Inspect live target files/symbols via `batch_inspect` to confirm alignment with the blueprint. Report any drift or missing assumptions first.
4. Execute **ONLY ONE small phase/batch per turn** (`multi_surgical_edit` / targeted checks), update `session_checkpoint` if needed, then **STOP** and end with 1 short follow-up question + 2-4 choices (continue next phase / debug or re-check / handoff to Antigravity).
"#;

pub fn builtin_skills(disabled_norm: &HashSet<String>) -> Vec<SkillEntry> {
    let raw_md = format!(
        "---\nname: {BUILTIN_WORKFLOW_SKILL_NAME}\ndescription: {BUILTIN_WORKFLOW_SKILL_DESC}\n---\n{BUILTIN_WORKFLOW_SKILL_BODY}"
    );
    let body_md = BUILTIN_WORKFLOW_SKILL_BODY.trim().to_string();
    let catalog_tokens_est =
        ((BUILTIN_WORKFLOW_SKILL_NAME.len() + BUILTIN_WORKFLOW_SKILL_DESC.len() + 12) / 4).max(1);
    let body_tokens_est = (body_md.len() / 4).max(1);
    let enabled = !disabled_norm.contains(BUILTIN_WORKFLOW_SKILL_NAME);

    vec![SkillEntry {
        summary: SkillSummary {
            name: BUILTIN_WORKFLOW_SKILL_NAME.to_string(),
            description: BUILTIN_WORKFLOW_SKILL_DESC.to_string(),
            scope: SkillScope::Global,
            source_label: "builtin:airlock".to_string(),
            plugin_id: None,
            enabled,
            disable_model_invocation: false,
            dir_path: "<builtin:airlock>".to_string(),
            resource_files: Vec::new(),
            allowed_tools: Vec::new(),
            catalog_tokens_est,
            body_tokens_est,
            parse_error: None,
        },
        canon_dir: PathBuf::new(),
        plugin_root: None,
        body_md,
        raw_md,
    }]
}

pub fn inject_builtin_skills(entries: &mut Vec<SkillEntry>, disabled_skills: &HashSet<String>) {
    let disabled_norm: HashSet<String> = disabled_skills
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    let mut valid_names: HashSet<String> = entries
        .iter()
        .filter(|e| e.summary.parse_error.is_none())
        .map(|e| e.summary.name.to_ascii_lowercase())
        .collect();

    let mut added = false;
    for b in builtin_skills(&disabled_norm) {
        let key = b.summary.name.to_ascii_lowercase();
        if !valid_names.contains(&key) {
            valid_names.insert(key);
            entries.push(b);
            added = true;
        }
    }

    if added {
        let mut valid: Vec<SkillEntry> = Vec::with_capacity(entries.len());
        let mut broken: Vec<SkillEntry> = Vec::new();
        for e in entries.drain(..) {
            if e.summary.parse_error.is_some() {
                broken.push(e);
            } else {
                valid.push(e);
            }
        }
        valid.sort_by(|a, b| a.summary.name.cmp(&b.summary.name));
        valid.extend(broken);
        *entries = valid;
    }
}

/// Scans skills bundled directly inside a staging/local plugin directory during install preview.
pub fn scan_plugin_skills_preview(plugin_root: &Path) -> Vec<SkillSummary> {
    let mut valid_map: BTreeMap<String, SkillEntry> = BTreeMap::new();
    let mut broken_list: Vec<SkillEntry> = Vec::new();
    let empty_disabled = HashSet::new();
    scan_plugin_skill_containers(
        plugin_root,
        "plugin:preview",
        None,
        &empty_disabled,
        &mut valid_map,
        &mut broken_list,
    );
    let mut out: Vec<SkillSummary> = valid_map.into_values().map(|e| e.summary).collect();
    out.extend(broken_list.into_iter().map(|e| e.summary));
    out
}

pub fn read_skill_resource_file(skill: &SkillEntry, rel_path: &str) -> Result<String, String> {
    if skill.summary.source_label.starts_with("builtin:") || skill.canon_dir.as_os_str().is_empty() {
        return Err(format!(
            "Skill bawaan '{}' tidak memiliki file resource eksternal.",
            skill.summary.name
        ));
    }
    let trimmed = rel_path.trim();
    if trimmed.is_empty() {
        return Err("Parameter 'path' resource skill wajib diisi.".to_string());
    }
    if trimmed.contains('\0') {
        return Err("Path resource mengandung karakter NUL tidak valid.".to_string());
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err(format!(
            "Path resource absolut ditolak ('{trimmed}'). Gunakan path relatif di dalam folder skill."
        ));
    }
    let bytes_path = trimmed.as_bytes();
    if bytes_path.len() >= 2 && bytes_path[0].is_ascii_alphabetic() && bytes_path[1] == b':' {
        return Err(format!(
            "Path resource drive absolut ditolak ('{trimmed}'). Gunakan path relatif di dalam folder skill."
        ));
    }
    for comp in Path::new(trimmed).components() {
        if matches!(
            comp,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(format!(
                "Path traversal/absolut ditolak pada resource skill ('{trimmed}')."
            ));
        }
    }

    let norm = trimmed.replace('\\', "/");
    let resolved = if let Some(plugin_rel) = norm.strip_prefix("plugin/") {
        let Some(plg_root) = skill.plugin_root.as_ref() else {
            return Err(format!(
                "Skill '{}' tidak memiliki plugin root untuk membaca '{trimmed}'.",
                skill.summary.name
            ));
        };
        let allowed_top = PLUGIN_SHARED_RESOURCE_SUBDIRS.iter().any(|sub| {
            plugin_rel == *sub || plugin_rel.starts_with(&format!("{sub}/"))
        });
        if !allowed_top {
            return Err(format!(
                "Akses shared plugin resource '{trimmed}' ditolak: hanya direktori {:?} yang diizinkan.",
                PLUGIN_SHARED_RESOURCE_SUBDIRS
            ));
        }
        resolve_jailed_path(plg_root, plugin_rel)?
    } else {
        resolve_jailed_path(&skill.canon_dir, trimmed)?
    };
    let meta = fs::metadata(&resolved)
        .map_err(|e| format!("Resource skill '{trimmed}' tidak ditemukan: {e}"))?;
    if !meta.is_file() {
        return Err(format!("Resource '{trimmed}' bukan file reguler."));
    }
    if meta.len() > MAX_RESOURCE_READ_BYTES {
        return Err(format!(
            "Resource '{trimmed}' melebihi batas 128 KB ({} bytes).",
            meta.len()
        ));
    }

    let raw_bytes =
        fs::read(&resolved).map_err(|e| format!("Gagal membaca resource '{trimmed}': {e}"))?;
    if raw_bytes.contains(&0) {
        return Err(format!("Resource '{trimmed}' adalah file biner (ditolak)."));
    }
    String::from_utf8(raw_bytes)
        .map_err(|_| format!("Resource '{trimmed}' bukan teks UTF-8 yang valid."))
}

pub fn create_global_skill_file(
    global_skills_dir: &Path,
    name: &str,
    description: &str,
    body_md: &str,
) -> Result<PathBuf, String> {
    let name_clean = name.trim();
    if !is_valid_skill_name(name_clean) {
        return Err(format!(
            "Nama skill '{name_clean}' tidak valid (gunakan 1-64 huruf kecil a-z, angka 0-9, dan '-'; tanpa '--')."
        ));
    }
    let desc_clean = description
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if desc_clean.is_empty() {
        return Err("Deskripsi skill tidak boleh kosong.".to_string());
    }

    let skill_dir = global_skills_dir.join(name_clean);
    let skill_md = skill_dir.join("SKILL.md");
    if skill_md.exists() {
        return Err(format!(
            "Global skill '{name_clean}' sudah ada di '{}'.",
            clean_path_string(&skill_md)
        ));
    }

    fs::create_dir_all(&skill_dir)
        .map_err(|e| format!("Gagal membuat folder global skill '{name_clean}': {e}"))?;

    let quoted_desc = desc_clean.replace('\\', "\\\\").replace('"', "\\\"");
    let body_trimmed = body_md.trim();
    let final_body = if body_trimmed.is_empty() {
        format!("# {name_clean}\n\n{desc_clean}\n")
    } else {
        format!("{body_trimmed}\n")
    };

    let content = format!(
        "---\nname: {name_clean}\ndescription: \"{quoted_desc}\"\n---\n\n{final_body}"
    );
    fs::write(&skill_md, content)
        .map_err(|e| format!("Gagal menulis SKILL.md '{name_clean}': {e}"))?;

    Ok(skill_md)
}

pub fn build_skills_prompt_section(skills: &[SkillEntry]) -> Option<String> {
    let active: Vec<&SkillEntry> = skills
        .iter()
        .filter(|s| {
            s.summary.enabled
                && s.summary.parse_error.is_none()
                && !s.summary.disable_model_invocation
        })
        .collect();
    if active.is_empty() {
        return None;
    }

    let desc_cap = if active.len() > 40 { 140 } else { 220 };
    let mut lines = Vec::with_capacity(active.len() + 2);
    lines.push("### Available Skills (Progressive Disclosure)".to_string());
    lines.push(
        "Before solving specialized tasks, check if a skill below applies and call `load_skill(name)` to load its full instructions:"
            .to_string(),
    );

    for entry in active {
        let single_line = entry
            .summary
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace('`', "'");
        let clipped: String = if single_line.chars().count() > desc_cap {
            let mut s: String = single_line.chars().take(desc_cap.saturating_sub(3)).collect();
            s.push_str("...");
            s
        } else {
            single_line
        };
        lines.push(format!("- `{}`: {}", entry.summary.name, clipped));
    }

    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frontmatter_parsing_crlf_and_validation() {
        let valid_crlf = "\u{feff}---\r\nname: tauri-security-audit\r\ndescription: >\r\n  Audit Tauri v2 IPC commands\r\n  and filesystem jail boundaries.\r\nlicense: MIT\r\nallowed-tools: project_overview smart_search read_file_range\r\nmetadata:\r\n  owner: security-team\r\n---\r\n\r\n# Audit Instructions\r\nCheck CSP.\r\n";
        let parsed = parse_skill_markdown(valid_crlf, None).unwrap();
        assert_eq!(parsed.name, "tauri-security-audit");
        assert_eq!(
            parsed.description,
            "Audit Tauri v2 IPC commands and filesystem jail boundaries."
        );
        assert_eq!(
            parsed.allowed_tools,
            vec!["project_overview", "smart_search", "read_file_range"]
        );
        assert_eq!(
            parsed.metadata.get("owner").map(String::as_str),
            Some("security-team")
        );
        assert!(parsed.body_md.contains("# Audit Instructions"));

        // Fallback to folder name when name is omitted in frontmatter
        let omit_name = "---\ndescription: Fallback folder name skill.\n---\nBody text";
        let fallback_parsed = parse_skill_markdown(omit_name, Some("My_Cool_Skill")).unwrap();
        assert_eq!(fallback_parsed.name, "my-cool-skill");

        // Invalid names and corrupted frontmatter return Err cleanly
        assert!(parse_skill_markdown("no frontmatter", Some("good")).is_err());
        assert!(parse_skill_markdown("---\nname: bad--name\ndescription: ok\n---\n", None).is_err());
        assert!(parse_skill_markdown("---\nname: valid-name\n---\n", None).is_err());
    }

    #[test]
    fn test_discovery_precedence_workspace_overrides_plugin_and_global() {
        let base = std::env::temp_dir().join("smart_mcp_skills_precedence_test");
        let _ = fs::remove_dir_all(&base);

        let ws_root = base.join("ws_alpha");
        let plg_root = base.join("plugin_root");
        let global_root = base.join("global_skills");

        fs::create_dir_all(ws_root.join(".airlock/skills/shared-skill/references")).unwrap();
        fs::create_dir_all(plg_root.join("skills/shared-skill")).unwrap();
        fs::create_dir_all(plg_root.join("skills/plugin-only")).unwrap();
        fs::create_dir_all(global_root.join("shared-skill")).unwrap();
        fs::create_dir_all(global_root.join("zebra-global")).unwrap();
        fs::create_dir_all(global_root.join("broken-skill")).unwrap();

        fs::write(
            ws_root.join(".airlock/skills/shared-skill/SKILL.md"),
            "---\nname: shared-skill\ndescription: From workspace override\n---\nWorkspace body",
        )
        .unwrap();
        fs::write(
            ws_root.join(".airlock/skills/shared-skill/references/guide.md"),
            "# Ref\n",
        )
        .unwrap();

        fs::write(
            plg_root.join("skills/shared-skill/SKILL.md"),
            "---\nname: shared-skill\ndescription: From plugin\n---\nPlugin body",
        )
        .unwrap();
        fs::write(
            plg_root.join("skills/plugin-only/SKILL.md"),
            "---\nname: plugin-only\ndescription: Only in plugin\n---\nBody",
        )
        .unwrap();

        fs::write(
            global_root.join("shared-skill/SKILL.md"),
            "---\nname: shared-skill\ndescription: From global\n---\nGlobal body",
        )
        .unwrap();
        fs::write(
            global_root.join("zebra-global/SKILL.md"),
            "---\nname: zebra-global\ndescription: Last alphabetically\n---\nBody",
        )
        .unwrap();
        fs::write(
            global_root.join("broken-skill/SKILL.md"),
            "not valid yaml frontmatter",
        )
        .unwrap();

        let plugins = vec![InstalledPluginRecord {
            id: "plg_demo_123456".to_string(),
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            description: "Demo".to_string(),
            source: "local:test".to_string(),
            install_dir: clean_path_string(&plg_root),
            enabled: true,
            trusted: true,
            manifest_sha256: "abc".to_string(),
            mcp_server_ids: vec![],
            required_secrets: vec![],
            installed_at: "2026-11-24".to_string(),
        }];

        let mut disabled = HashSet::new();
        disabled.insert("zebra-global".to_string());

        let discovered = discover_skills(
            &[("alpha".to_string(), ws_root.clone())],
            &plugins,
            Some(&global_root),
            &disabled,
        );

        let valid: Vec<&SkillEntry> = discovered
            .iter()
            .filter(|s| s.summary.parse_error.is_none())
            .collect();
        let names: Vec<&str> = valid.iter().map(|s| s.summary.name.as_str()).collect();
        // Deterministic alphabetical order of valid skills
        assert_eq!(names, vec!["plugin-only", "shared-skill", "zebra-global"]);

        let shared = valid
            .iter()
            .find(|s| s.summary.name == "shared-skill")
            .unwrap();
        assert_eq!(shared.summary.scope, SkillScope::Workspace);
        assert_eq!(shared.summary.source_label, "workspace:alpha");
        assert!(shared.body_md.contains("Workspace body"));
        assert_eq!(shared.summary.resource_files, vec!["references/guide.md"]);

        let zebra = valid
            .iter()
            .find(|s| s.summary.name == "zebra-global")
            .unwrap();
        assert!(!zebra.summary.enabled);

        let broken = discovered
            .iter()
            .find(|s| s.summary.parse_error.is_some())
            .expect("corrupt SKILL.md should be recorded with parse_error");
        assert!(!broken.summary.enabled);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_read_skill_resource_blocks_traversal_and_absolute_paths() {
        let base = std::env::temp_dir().join("smart_mcp_skill_resource_jail_test");
        let _ = fs::remove_dir_all(&base);
        let skill_dir = base.join("my-skill");
        fs::create_dir_all(skill_dir.join("references")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Safe reader\n---\nBody",
        )
        .unwrap();
        fs::write(
            skill_dir.join("references/checklist.md"),
            "- Item 1\n- Item 2\n",
        )
        .unwrap();
        fs::write(base.join("outside_secret.txt"), "TOP_SECRET").unwrap();

        let discovered = discover_skills(&[], &[], Some(&base), &HashSet::new());
        assert_eq!(discovered.len(), 1);
        let skill = &discovered[0];

        let ok = read_skill_resource_file(skill, "references/checklist.md").unwrap();
        assert!(ok.contains("- Item 1"));

        assert!(read_skill_resource_file(skill, "../outside_secret.txt").is_err());
        assert!(read_skill_resource_file(skill, "../../secrets.dpapi").is_err());
        assert!(read_skill_resource_file(skill, r"C:\Windows\System32\drivers\etc\hosts").is_err());
        assert!(read_skill_resource_file(skill, "/etc/passwd").is_err());
        assert!(read_skill_resource_file(skill, r"\\server\share\secret.txt").is_err());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_disable_model_invocation_and_plugin_shared_resources() {
        let base = std::env::temp_dir().join("smart_mcp_skill_plugin_shared_res_test");
        let _ = fs::remove_dir_all(&base);
        let plg_dir = base.join("plg_egm_like");
        fs::create_dir_all(plg_dir.join("agents")).unwrap();
        fs::create_dir_all(plg_dir.join("skills/egm-review/examples")).unwrap();
        fs::create_dir_all(plg_dir.join("skills/auto-guide")).unwrap();

        fs::write(plg_dir.join("plugin.json"), r#"{"name":"egm-like"}"#).unwrap();
        fs::write(
            plg_dir.join("agents/design_critic.md"),
            "# Design Critic Persona\nChallenge assumptions.",
        )
        .unwrap();
        fs::write(
            plg_dir.join("skills/egm-review/SKILL.md"),
            "---\nname: egm-review\ndescription: Manual review workflow\ndisable-model-invocation: true\n---\nRun review.",
        )
        .unwrap();
        fs::write(
            plg_dir.join("skills/egm-review/examples/sample.md"),
            "Sample review",
        )
        .unwrap();
        fs::write(
            plg_dir.join("skills/auto-guide/SKILL.md"),
            "---\nname: auto-guide\ndescription: Auto catalog skill\n---\nAuto body",
        )
        .unwrap();

        let plugins = vec![InstalledPluginRecord {
            id: "plg_egm_123456".to_string(),
            name: "egm-like".to_string(),
            version: "0.1.0".to_string(),
            description: "EGM".to_string(),
            source: "local:test".to_string(),
            install_dir: clean_path_string(&plg_dir),
            enabled: true,
            trusted: true,
            manifest_sha256: "hash".to_string(),
            mcp_server_ids: vec![],
            required_secrets: vec![],
            installed_at: String::new(),
        }];

        let discovered = discover_skills(&[], &plugins, None, &HashSet::new());
        let review = discovered
            .iter()
            .find(|s| s.summary.name == "egm-review")
            .unwrap();
        assert!(review.summary.disable_model_invocation);
        assert!(review
            .summary
            .resource_files
            .contains(&"examples/sample.md".to_string()));
        assert!(review
            .summary
            .resource_files
            .contains(&"plugin/agents/design_critic.md".to_string()));

        // Auto prompt catalog omits disable-model-invocation skills while keeping auto-guide
        let prompt_block = build_skills_prompt_section(&discovered).unwrap();
        assert!(prompt_block.contains("auto-guide"));
        assert!(!prompt_block.contains("egm-review"));

        // Shared plugin/agents resource is readable, while plugin/plugin.json is blocked
        let critic = read_skill_resource_file(review, "plugin/agents/design_critic.md").unwrap();
        assert!(critic.contains("Design Critic Persona"));
        assert!(read_skill_resource_file(review, "plugin/plugin.json").is_err());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_builtin_workflow_skill_injection_override_and_toggle() {
        let base = std::env::temp_dir().join("smart_mcp_builtin_workflow_skill_test");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        // 1. Default injection adds enabled airlock-blueprint-workflow
        let mut list = discover_skills(&[], &[], None, &HashSet::new());
        inject_builtin_skills(&mut list, &HashSet::new());
        let wf = list
            .iter()
            .find(|s| s.summary.name == BUILTIN_WORKFLOW_SKILL_NAME)
            .expect("built-in workflow skill should be injected");
        assert!(wf.summary.enabled);
        assert_eq!(wf.summary.source_label, "builtin:airlock");
        assert!(wf.body_md.contains("Turn-End Follow-Up Choices Protocol"));
        assert!(wf.body_md.contains("WEBMCP_NEXT_SESSION_PROMPT"));
        assert!(wf.body_md.contains("ANTIGRAVITY_HANDOFF_PROMPT"));
        assert!(
            read_skill_resource_file(wf, "README.md").is_err(),
            "built-in virtual skill must reject external resource reads"
        );

        // 2. disabled_skills marks built-in skill disabled without crashing discovery
        let mut disabled = HashSet::new();
        disabled.insert(BUILTIN_WORKFLOW_SKILL_NAME.to_string());
        let mut list_dis = discover_skills(&[], &[], None, &disabled);
        inject_builtin_skills(&mut list_dis, &disabled);
        let wf_dis = list_dis
            .iter()
            .find(|s| s.summary.name == BUILTIN_WORKFLOW_SKILL_NAME)
            .unwrap();
        assert!(!wf_dis.summary.enabled);

        // 3. Workspace skill with the same name cleanly overrides the built-in entry
        let ws_override = base.join(".airlock/skills").join(BUILTIN_WORKFLOW_SKILL_NAME);
        fs::create_dir_all(&ws_override).unwrap();
        fs::write(
            ws_override.join("SKILL.md"),
            format!("---\nname: {BUILTIN_WORKFLOW_SKILL_NAME}\ndescription: Custom workspace workflow override.\n---\n# Custom Override"),
        )
        .unwrap();
        let ws_roots = vec![("repo".to_string(), base.clone())];
        let mut list_ov = discover_skills(&ws_roots, &[], None, &HashSet::new());
        inject_builtin_skills(&mut list_ov, &HashSet::new());
        let matching: Vec<_> = list_ov
            .iter()
            .filter(|s| s.summary.name == BUILTIN_WORKFLOW_SKILL_NAME)
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].summary.source_label, "workspace:repo");
        assert!(matching[0].body_md.contains("Custom Override"));

        let _ = fs::remove_dir_all(&base);
    }
}
