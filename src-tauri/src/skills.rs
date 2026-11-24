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
        metadata,
        body_md,
    })
}

fn scan_resource_dir_recursive(
    canon_skill_dir: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth > 4 || out.len() >= 100 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for path in children {
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if fname.starts_with('.') {
            continue;
        }
        let Ok(canon_entry) = fs::canonicalize(&path) else {
            continue;
        };
        if !canon_entry.starts_with(canon_skill_dir) {
            continue;
        }
        let Ok(meta) = fs::metadata(&canon_entry) else {
            continue;
        };
        if meta.is_dir() {
            scan_resource_dir_recursive(canon_skill_dir, &canon_entry, depth + 1, out);
        } else if meta.is_file() && meta.len() <= MAX_RESOURCE_SCAN_BYTES {
            if let Ok(rel) = canon_entry.strip_prefix(canon_skill_dir) {
                let rel_slash = rel.to_string_lossy().replace('\\', "/");
                if !rel_slash.is_empty() {
                    out.push(rel_slash);
                }
            }
        }
    }
}

fn scan_skill_resources(canon_skill_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for sub in ["references", "scripts", "assets"] {
        let sub_path = canon_skill_dir.join(sub);
        if let Ok(canon_sub) = fs::canonicalize(&sub_path) {
            if canon_sub.starts_with(canon_skill_dir) && canon_sub.is_dir() {
                scan_resource_dir_recursive(canon_skill_dir, &canon_sub, 1, &mut files);
            }
        }
    }
    files.sort();
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
    if let Some(root) = jail_root {
        let canon_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if !canon_dir.starts_with(&canon_root) {
            return None;
        }
    }
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
                    dir_path: clean_dir,
                    resource_files: Vec::new(),
                    allowed_tools: Vec::new(),
                    catalog_tokens_est: 0,
                    body_tokens_est: 0,
                    parse_error: Some(format!("Gagal membaca metadata SKILL.md: {e}")),
                },
                canon_dir,
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
                dir_path: clean_dir,
                resource_files: Vec::new(),
                allowed_tools: Vec::new(),
                catalog_tokens_est: 0,
                body_tokens_est: 0,
                parse_error: Some("Ukuran SKILL.md melebihi batas maksimum 256 KB.".to_string()),
            },
            canon_dir,
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
                    dir_path: clean_dir,
                    resource_files: Vec::new(),
                    allowed_tools: Vec::new(),
                    catalog_tokens_est: 0,
                    body_tokens_est: 0,
                    parse_error: Some(format!("Gagal membaca SKILL.md sebagai UTF-8: {e}")),
                },
                canon_dir,
                body_md: String::new(),
                raw_md: String::new(),
            });
        }
    };

    let resource_files = scan_skill_resources(&canon_dir);
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
                    dir_path: clean_dir,
                    resource_files,
                    allowed_tools: parsed.allowed_tools,
                    catalog_tokens_est,
                    body_tokens_est,
                    parse_error: None,
                },
                canon_dir,
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
                    dir_path: clean_dir,
                    resource_files,
                    allowed_tools: Vec::new(),
                    catalog_tokens_est: 0,
                    body_tokens_est: 0,
                    parse_error: Some(err),
                },
                canon_dir,
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
        let container = plg_dir.join("skills");
        let src_label = format!("plugin:{}", plugin.name);
        scan_skills_container(
            &container,
            Some(&plg_dir),
            SkillScope::Plugin,
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

/// Scans skills bundled directly inside a staging/local plugin directory during install preview.
pub fn scan_plugin_skills_preview(plugin_root: &Path) -> Vec<SkillSummary> {
    let mut valid_map: BTreeMap<String, SkillEntry> = BTreeMap::new();
    let mut broken_list: Vec<SkillEntry> = Vec::new();
    let empty_disabled = HashSet::new();
    let skills_dir = plugin_root.join("skills");
    scan_skills_container(
        &skills_dir,
        Some(plugin_root),
        SkillScope::Plugin,
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

    let resolved = resolve_jailed_path(&skill.canon_dir, trimmed)?;
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
        .filter(|s| s.summary.enabled && s.summary.parse_error.is_none())
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
}
