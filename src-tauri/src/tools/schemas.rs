use crate::config::McpToolProfile;
use serde_json::{json, Value};

/// Shared tool metadata used by both MCP `tools/list` and Gemini `functionDeclarations`.
pub fn mcp_tools_list() -> Value {
    let mut items = match json!([
        {
            "name": "list_workspaces",
            "description": "Lists all mounted enabled workspaces with their @alias, path, default flag, and effective guardrail mode.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "project_overview",
            "description": "Returns the jailed project directory tree (ignoring node_modules/.git/target) and short git status in 1 call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID (defaults to active default workspace)" },
                    "max_depth": { "type": "integer", "description": "Max folder depth (default 3, max 6)" }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "smart_search",
            "description": "Fast ripgrep-style text/regex search across project files returning file paths, line numbers, and 2 lines of context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "query": { "type": "string", "description": "Search text or regex pattern" },
                    "path": { "type": "string", "description": "Optional relative subfolder or file" },
                    "case_insensitive": { "type": "boolean", "description": "Ignore case (default false)" },
                    "max_results": { "type": "integer", "description": "Max matches (default 30)" }
                },
                "required": ["query"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "read_file_range",
            "description": "Reads specific line ranges from one file or multiple files in batch_paths (even across mounted workspaces) with line numbers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string", "description": "Relative file path" },
                    "start_line": { "type": "integer", "description": "1-based start line (default 1)" },
                    "end_line": { "type": "integer", "description": "1-based end line (default start_line + 120)" },
                    "batch_paths": {
                        "type": "array",
                        "description": "Optional array of { workspace?, path, start_line, end_line } to read multiple slices in 1 turn",
                        "items": {
                            "type": "object",
                            "properties": {
                                "workspace": { "type": "string", "description": "Optional workspace alias override for this item" },
                                "path": { "type": "string" },
                                "start_line": { "type": "integer" },
                                "end_line": { "type": "integer" }
                            },
                            "required": ["path"]
                        }
                    }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "batch_inspect",
            "description": "Compound read-only inspection: runs optional instructions/GEMINI.md rules, project_overview, up to 3 find_patterns, up to 8 code_outline calls, up to 4 smart_search queries, up to 6 read_slices, and up to 6 symbol_reads concurrently in 1 turn.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "include_instructions": { "type": "boolean", "description": "Include global and workspace GEMINI.md rules (default false; auto-injected once on first WebMCP batch_inspect per workspace)" },
                    "include_overview": { "type": "boolean", "description": "Include directory overview + git status (default false)" },
                    "overview_depth": { "type": "integer", "description": "Overview folder depth (default 2, max 4)" },
                    "find_patterns": {
                        "type": "array",
                        "description": "Up to 3 glob/substring filename find queries",
                        "items": {
                            "type": "object",
                            "properties": {
                                "pattern": { "type": "string" },
                                "path": { "type": "string" },
                                "max_results": { "type": "integer" }
                            },
                            "required": ["pattern"]
                        }
                    },
                    "outline_paths": {
                        "type": "array",
                        "description": "Up to 8 relative file paths for symbol outline extraction",
                        "items": { "type": "string" }
                    },
                    "searches": {
                        "type": "array",
                        "description": "Up to 4 smart_search queries",
                        "items": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string" },
                                "path": { "type": "string" },
                                "case_insensitive": { "type": "boolean" },
                                "max_results": { "type": "integer" }
                            },
                            "required": ["query"]
                        }
                    },
                    "read_slices": {
                        "type": "array",
                        "description": "Up to 6 { workspace?, path, start_line?, end_line? } line-range file reads (default 120 lines, max 160 lines per slice on WebMCP)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "workspace": { "type": "string" },
                                "path": { "type": "string" },
                                "start_line": { "type": "integer" },
                                "end_line": { "type": "integer" }
                            },
                            "required": ["path"]
                        }
                    },
                    "symbol_reads": {
                        "type": "array",
                        "description": "Up to 6 AST-bounded symbol body reads { path, query, include_docs?, max_lines? }",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "query": { "type": "string" },
                                "include_docs": { "type": "boolean" },
                                "max_lines": { "type": "integer" }
                            },
                            "required": ["path", "query"]
                        }
                    }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "surgical_edit",
            "description": "Replaces exact search blocks inside a file without rewriting the whole file. Saves 75-85% tokens and saves a snapshot for 1-click revert.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string", "description": "Target relative file path" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "search": { "type": "string", "description": "Exact existing text block" },
                                "replace": { "type": "string", "description": "Replacement text block" },
                                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
                            },
                            "required": ["search", "replace"]
                        }
                    }
                },
                "required": ["path", "edits"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "multi_surgical_edit",
            "description": "Atomic multi-file surgical edit (1-8 files in one workspace): pre-flight validates EVERY search block and optional verify command before writing any file, records shared group revert snapshot, and optionally runs warm LSP check and verification.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "files": {
                        "type": "array",
                        "description": "List of 1 to 8 { path, edits } operations",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "edits": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "search": { "type": "string" },
                                            "replace": { "type": "string" },
                                            "replace_all": { "type": "boolean" }
                                        },
                                        "required": ["search", "replace"]
                                    }
                                }
                            },
                            "required": ["path", "edits"]
                        }
                    },
                    "post_check_lsp": {
                        "type": "boolean",
                        "description": "Run warm-only fast LSP diagnostic check on edited files (skips without cold-start if server is not already warm)"
                    },
                    "verify_command": {
                        "type": "string",
                        "description": "Optional verification command to execute right after editing (validated & HITL-checked before writing any file)"
                    },
                    "verify_commands": {
                        "type": "array",
                        "description": "Optional pipeline of 1 to 5 verification commands executed sequentially after editing",
                        "items": { "type": "string" }
                    },
                    "auto_promote_secs": {
                        "type": "integer",
                        "description": "Auto-promote verification to background job after N seconds (1-25; default 18 on WebMCP)"
                    }
                },
                "required": ["files"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "patch_and_verify",
            "description": "Macro tool: applies surgical_edit blocks (saving rollback snapshot first) AND runs a verification command (e.g., npm test / cargo check) with optional async job auto-promotion.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "search": { "type": "string" },
                                "replace": { "type": "string" },
                                "replace_all": { "type": "boolean" }
                            },
                            "required": ["search", "replace"]
                        }
                    },
                    "verify_command": { "type": "string", "description": "Verification shell command to run right after patching" },
                    "auto_promote_secs": { "type": "integer", "description": "Auto-promote verify command to async background job after N seconds (1-25; default 18 for WebMCP)" }
                },
                "required": ["path", "edits", "verify_command"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "run_command_compressed",
            "description": "Executes a shell command in workspace and compresses logs via Token Guard. On WebMCP (or when auto_promote_secs is set), long commands auto-promote to a background job instead of timing out.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 18; max 30 sync or 1800 if promoted)" },
                    "max_lines": { "type": "integer", "description": "Max compressed lines (default 60)" },
                    "auto_promote_secs": { "type": "integer", "description": "Auto-promote to async background job if still running after N seconds (1-25; default 18 for WebMCP)" }
                },
                "required": ["command"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "write_file_jailed",
            "description": "Creates or overwrites a file strictly inside workspace_root while saving a rollback snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "find_files",
            "description": "Fast glob-based file path discovery ignoring node_modules/.git/target/dist without reading file contents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "pattern": { "type": "string", "description": "Wildcard glob pattern (e.g. *.rs, *auth*, src/*.ts)" },
                    "path": { "type": "string", "description": "Optional relative subfolder" },
                    "max_results": { "type": "integer", "description": "Max files returned (default 80, max 300)" }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "code_outline",
            "description": "Extracts symbol declarations (functions, structs, classes, interfaces, headings) with line numbers before reading slices.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string", "description": "Target relative file path" },
                    "max_symbols": { "type": "integer", "description": "Max symbols (default 120, max 400)" }
                },
                "required": ["path"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "read_symbol",
            "description": "Reads the exact AST definition (including doc comments/decorators) of a single symbol (e.g. 'AppState::snapshot' or 'Client.connect') from a source file without guessing line ranges.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string", "description": "Target relative file path" },
                    "symbol": { "type": "string", "description": "Symbol name or qualified name (e.g. 'MyStruct::method' or 'helper')" },
                    "include_docs": { "type": "boolean", "description": "Include leading doc comments/attributes/decorators (default true)" },
                    "max_lines": { "type": "integer", "description": "Max lines returned for the symbol body (default 200, max 500)" }
                },
                "required": ["path", "symbol"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }
    ]) {
        Value::Array(arr) => arr,
        _ => Vec::new(),
    };
    if let Value::Array(rest) = json!([
        {
            "name": "lsp_diagnostics",
            "description": "Queries active Language Server diagnostics (compiler/type errors & warnings) for a file or workspace on demand (zero background bloat).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string", "description": "Optional relative file path to analyze immediately" },
                    "severity": {
                        "type": "string",
                        "enum": ["error", "warning", "all"],
                        "description": "Filter severity (default 'all')"
                    }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "lsp_definition",
            "description": "Jumps to the semantic symbol definition at 1-based line and character via on-demand Language Server (returns jailed path & preview).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string", "description": "Target relative file path" },
                    "line": { "type": "integer", "description": "1-based line number" },
                    "character": { "type": "integer", "description": "1-based character/column offset" }
                },
                "required": ["path", "line", "character"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "lsp_references",
            "description": "Finds semantic cross-file references to the symbol at 1-based line and character via Language Server.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string", "description": "Target relative file path" },
                    "line": { "type": "integer", "description": "1-based line number" },
                    "character": { "type": "integer", "description": "1-based character/column offset" },
                    "max_results": { "type": "integer", "description": "Max references returned (default 30, max 100)" }
                },
                "required": ["path", "line", "character"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "lsp_hover",
            "description": "Retrieves resolved type signature and documentation on hover at 1-based line and character via Language Server.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "path": { "type": "string", "description": "Target relative file path" },
                    "line": { "type": "integer", "description": "1-based line number" },
                    "character": { "type": "integer", "description": "1-based character/column offset" }
                },
                "required": ["path", "line", "character"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "git_ops",
            "description": "Safe structured Git operations with compressed output: status, diff, diff_staged, log, branch, show, blame, stash_list, add, commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "op": {
                        "type": "string",
                        "enum": ["status", "diff", "diff_staged", "log", "branch", "show", "blame", "stash_list", "add", "commit"]
                    },
                    "path": { "type": "string", "description": "Optional relative file/folder path" },
                    "message": { "type": "string", "description": "Commit message (required for op=commit)" },
                    "target": { "type": "string", "description": "Commit/ref for op=show" },
                    "limit": { "type": "integer", "description": "Max commits for op=log (default 15, max 50)" }
                },
                "required": ["op"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "file_ops",
            "description": "Jailed file management (mkdir, move, delete) with snapshot backup for 1-click revert on delete/move.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "action": { "type": "string", "enum": ["mkdir", "move", "delete"] },
                    "path": { "type": "string", "description": "Target relative path" },
                    "destination": { "type": "string", "description": "Destination relative path (required for action=move)" }
                },
                "required": ["action", "path"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "start_command_job",
            "description": "Starts a long-running shell command asynchronously in non-interactive mode (CI=true, GIT_TERMINAL_PROMPT=0, PYTHONUNBUFFERED=1) and returns job_id immediately (<50ms). Follow with poll_command_job (which waits up to 22s per call automatically; never run sleep).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "command": { "type": "string", "description": "Shell command to run in background" },
                    "label": { "type": "string", "description": "Optional short label" },
                    "timeout_secs": { "type": "integer", "description": "Max runtime seconds (default 300, max 1800)" }
                },
                "required": ["command"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "poll_command_job",
            "description": "Polls status and compressed incremental output from a background job. Automatically long-polls up to wait_secs (default 22s for WebMCP) and wakes immediately when the job finishes, avoiding spin-polling or sleep commands.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "from_line": { "type": "integer", "description": "0-based line offset (default 0)" },
                    "max_lines": { "type": "integer", "description": "Max compressed lines (default 80)" },
                    "wait_secs": { "type": "integer", "description": "Long-poll wait up to N seconds (0-26) for completion before returning (default 22 for WebMCP, 0 for local)" }
                },
                "required": ["job_id"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "stop_command_job",
            "description": "Stops a running background command job and terminates its process tree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" }
                },
                "required": ["job_id"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "list_skills",
            "description": "Lists active Agent Skills (SKILL.md) discovered across workspace, enabled plugins, and global skills. Supports optional query, scope, and compact filtering.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Optional case-insensitive filter on skill name or description" },
                    "scope": { "type": "string", "enum": ["workspace", "plugin", "global"], "description": "Optional scope filter" },
                    "compact": { "type": "boolean", "description": "If true, emits single-line compact catalog entries so large catalogs fit within WebMCP output limits" }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "load_skill",
            "description": "Loads the full SKILL.md instructions and bundled resource file list for a specific skill by name (progressive disclosure).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Exact skill name from list_skills / system prompt catalog" }
                },
                "required": ["name"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "read_skill_resource",
            "description": "Reads a jailed text resource file (e.g. references/guide.md or scripts/helper.py) bundled inside an active skill directory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Active skill name" },
                    "path": { "type": "string", "description": "Relative path inside the skill folder (e.g. references/checklist.md)" }
                },
                "required": ["name", "path"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "upsert_artifact",
            "description": "Creates or updates a structured session artifact (implementation_plan, task_list, walkthrough, or review_note) in Airlock's Artifacts panel without polluting git status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "kind": { "type": "string", "description": "Artifact type: implementation_plan | task_list | walkthrough | review_note" },
                    "title": { "type": "string", "description": "Concise human-readable title" },
                    "content_md": { "type": "string", "description": "Full Markdown document content (supports tables and task checkboxes)" }
                },
                "required": ["kind", "title", "content_md"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "request_plan_approval",
            "description": "Marks the current implementation_plan artifact as awaiting_approval and cleanly pauses execution so the user can review/comment/approve.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "artifact_id": { "type": "string", "description": "Optional specific artifact ID (defaults to session implementation_plan)" },
                    "summary": { "type": "string", "description": "Brief summary of what was planned for user review" }
                },
                "required": ["summary"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "spawn_subagent",
            "description": "Spawns a focused read-only subagent to investigate a question, audit files, or inspect another workspace in parallel without polluting the main context window. Returns a concise structured report.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "Specific research, audit, or verification goal" },
                    "workspace": { "type": "string", "description": "Optional target workspace alias" },
                    "focus_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional relative paths to inspect first"
                    },
                    "max_steps": { "type": "integer", "description": "Max tool steps for subagent (default 6, max 10)" }
                },
                "required": ["goal"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "run_command",
            "description": "Unified shell execution & job manager: action='run' (auto-promotes after 18s on WebMCP), 'start' (immediate background job), 'poll' (long-polls up to 22s automatically; NEVER use sleep), or 'stop'. Supports single 'command' or sequential 1-5 step 'commands' pipeline.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "action": {
                        "type": "string",
                        "enum": ["run", "start", "poll", "stop"],
                        "description": "Execution mode (default 'run')"
                    },
                    "command": { "type": "string", "description": "Single shell command (for action='run' or 'start')" },
                    "commands": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Sequential pipeline of 1 to 5 commands (stops on first failure)"
                    },
                    "label": { "type": "string", "description": "Optional label for background job" },
                    "job_id": { "type": "string", "description": "Target background job ID (required for action='poll' or 'stop')" },
                    "from_line": { "type": "integer", "description": "1-based/0-based cursor returned as NEXT_FROM_LINE when polling" },
                    "max_lines": { "type": "integer", "description": "Max log lines returned (default 60 for run, 80 for poll)" },
                    "wait_secs": { "type": "integer", "description": "Long-poll wait duration (0-26s; default 22 on WebMCP)" },
                    "timeout_secs": { "type": "integer", "description": "Execution timeout in seconds" },
                    "auto_promote_secs": { "type": "integer", "description": "Auto-promote sync command to background job after N seconds (1-25; default 18 on WebMCP)" }
                }
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "lsp_query",
            "description": "Unified IDE intelligence powered by workspace LSP: op='diagnostics' | 'definition' | 'references' | 'hover'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "op": {
                        "type": "string",
                        "enum": ["diagnostics", "definition", "references", "hover"],
                        "description": "LSP operation (default 'diagnostics')"
                    },
                    "path": { "type": "string", "description": "Relative file path (required for definition/references/hover, optional for workspace diagnostics)" },
                    "line": { "type": "integer", "description": "1-based line number (required for definition/references/hover)" },
                    "character": { "type": "integer", "description": "1-based character column (required for definition/references/hover)" },
                    "severity": { "type": "string", "enum": ["all", "error", "warning"], "description": "Severity filter for op='diagnostics'" },
                    "include_declaration": { "type": "boolean", "description": "Include declaration for op='references'" },
                    "max_results": { "type": "integer", "description": "Max locations for op='references'" }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "session_checkpoint",
            "description": "Saves, loads (with automatic git HEAD/dirty drift detection), or clears persistent per-workspace session state and emits a copy-pasteable ANTIGRAVITY_HANDOFF_PROMPT on save.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "op": {
                        "type": "string",
                        "enum": ["save", "load", "clear"],
                        "description": "Checkpoint operation (default 'load')"
                    },
                    "goal": { "type": "string", "description": "Current objective / task goal (for op='save')" },
                    "stage": {
                        "type": "string",
                        "enum": ["inspect", "blueprint", "implement"],
                        "description": "Workflow stage to tailor WEBMCP_NEXT_SESSION_PROMPT & ANTIGRAVITY_HANDOFF_PROMPT"
                    },
                    "blueprint_path": { "type": "string", "description": "Primary blueprint/plan file path for handoff prompts" },
                    "blueprint_paths": { "type": "array", "items": { "type": "string" }, "description": "Optional list of blueprint file paths (op='save')" },
                    "recommended_skills": { "type": "array", "items": { "type": "string" }, "description": "Optional skill names recommended for the next session" },
                    "status": { "type": "string", "description": "Status tag such as in_progress, blocked, or ready_to_verify (for op='save')" },
                    "decisions": { "type": "array", "items": { "type": "string" }, "description": "Key architectural/implementation decisions" },
                    "completed": { "type": "array", "items": { "type": "string" }, "description": "Completed milestones" },
                    "next_steps": { "type": "array", "items": { "type": "string" }, "description": "Remaining action items" },
                    "open_issues": { "type": "array", "items": { "type": "string" }, "description": "Blocking issues or risks" },
                    "touched_files": { "type": "array", "items": { "type": "string" }, "description": "Explicit modified files (auto-merged with recent Airlock audit edits)" },
                    "notes": { "type": "string", "description": "Concise freeform handoff notes" }
                }
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "delegate_local_agent",
            "description": "Delegates multi-step research (mode='read_only') or opt-in code edits (mode='execute') to Airlock's configured Local Agent with a bounded step budget. action='start' waits up to 18s on WebMCP then hands off cleanly to run_id; action='poll' long-polls up to 22s (never run sleep); action='cancel' aborts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Optional mounted workspace alias or ID" },
                    "action": {
                        "type": "string",
                        "enum": ["start", "poll", "cancel"],
                        "description": "Delegation action (default 'start')"
                    },
                    "goal": { "type": "string", "description": "Specific research or implementation goal (required for action='start')" },
                    "mode": {
                        "type": "string",
                        "enum": ["read_only", "execute"],
                        "description": "Sandbox mode: 'read_only' (default) or 'execute' (requires UI opt-in + non-read_only workspace)"
                    },
                    "focus_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional relative file paths to inspect first"
                    },
                    "max_steps": { "type": "integer", "description": "Bounded step limit (1-20, default 12)" },
                    "wait_secs": { "type": "integer", "description": "Sync wait window for start/poll (seconds)" },
                    "run_id": { "type": "string", "description": "Delegated run ID (required for action='poll' or 'cancel')" }
                }
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true
            }
        }
    ]) {
        items.extend(rest);
    }
    Value::Array(items)
}

/// Returns built-in tool schemas filtered/shaped for the active connection profile.
pub fn mcp_tools_list_for_profile(
    profile: McpToolProfile,
    enable_delegation: bool,
    expose_skills: bool,
    is_webmcp: bool,
) -> Value {
    let all = match mcp_tools_list() {
        Value::Array(arr) => arr,
        _ => Vec::new(),
    };

    let filtered: Vec<Value> = match profile {
        McpToolProfile::Full => all
            .into_iter()
            .filter(|t| {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if is_webmcp && name == "spawn_subagent" {
                    return false;
                }
                if name == "delegate_local_agent" && !(is_webmcp && enable_delegation) {
                    return false;
                }
                true
            })
            .collect(),
        McpToolProfile::Compact => {
            const COMPACT_TOOLS: &[&str] = &[
                "list_workspaces",
                "batch_inspect",
                "multi_surgical_edit",
                "write_file_jailed",
                "file_ops",
                "run_command",
                "git_ops",
                "lsp_query",
                "session_checkpoint",
            ];
            const SKILL_TOOLS: &[&str] = &["list_skills", "load_skill", "read_skill_resource"];

            all.into_iter()
                .filter(|t| {
                    let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if is_webmcp && name == "spawn_subagent" {
                        return false;
                    }
                    if name == "delegate_local_agent" {
                        return is_webmcp && enable_delegation;
                    }
                    COMPACT_TOOLS.contains(&name) || (expose_skills && SKILL_TOOLS.contains(&name))
                })
                .collect()
        }
    };

    Value::Array(filtered)
}

pub(crate) fn is_known_builtin_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "list_workspaces"
            | "project_overview"
            | "smart_search"
            | "read_file_range"
            | "batch_inspect"
            | "surgical_edit"
            | "multi_surgical_edit"
            | "patch_and_verify"
            | "run_command_compressed"
            | "run_command"
            | "write_file_jailed"
            | "find_files"
            | "code_outline"
            | "read_symbol"
            | "lsp_diagnostics"
            | "lsp_definition"
            | "lsp_references"
            | "lsp_hover"
            | "lsp_query"
            | "session_checkpoint"
            | "git_ops"
            | "file_ops"
            | "start_command_job"
            | "poll_command_job"
            | "stop_command_job"
            | "list_skills"
            | "load_skill"
            | "read_skill_resource"
            | "upsert_artifact"
            | "request_plan_approval"
            | "spawn_subagent"
            | "delegate_local_agent"
    )
}

pub(crate) fn normalized_arg_str(args: &Value, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub fn is_tool_read_only(tool_name: &str, args: &Value) -> bool {
    match tool_name {
        "list_workspaces"
        | "project_overview"
        | "smart_search"
        | "read_file_range"
        | "batch_inspect"
        | "find_files"
        | "code_outline"
        | "read_symbol"
        | "lsp_diagnostics"
        | "lsp_definition"
        | "lsp_references"
        | "lsp_hover"
        | "lsp_query"
        | "poll_command_job"
        | "list_skills"
        | "load_skill"
        | "read_skill_resource"
        | "spawn_subagent" => true,
        "delegate_local_agent" => {
            let action = normalized_arg_str(args, "action", "start");
            let mode = normalized_arg_str(args, "mode", "read_only");
            action == "poll" || (action == "start" && mode == "read_only")
        }
        "run_command" => normalized_arg_str(args, "action", "run") == "poll",
        "session_checkpoint" => normalized_arg_str(args, "op", "load") == "load",
        "git_ops" => {
            let op = normalized_arg_str(args, "op", "");
            matches!(
                op.as_str(),
                "status" | "diff" | "diff_staged" | "log" | "branch" | "show" | "blame" | "stash_list"
            )
        }
        _ => false,
    }
}

pub(crate) fn requires_hitl_in_safe_edit(tool_name: &str, args: &Value, is_read_only: bool) -> bool {
    if tool_name.contains("__") {
        return !is_read_only;
    }
    match tool_name {
        "run_command_compressed" | "patch_and_verify" | "start_command_job" => true,
        "run_command" => !matches!(
            normalized_arg_str(args, "action", "run").as_str(),
            "poll" | "stop"
        ),
        "file_ops" => normalized_arg_str(args, "action", "") == "delete",
        "git_ops" => normalized_arg_str(args, "op", "") == "commit",
        _ => false,
    }
}
