use super::*;

#[test]
fn build_system_prompt_includes_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("brave-search"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Web search via Brave API".to_owned(),
            file_path: PathBuf::from("/skills/brave-search/SKILL.md"),
            add_to_prompt: true,
        },
    );
    let prompt = build_system_prompt(&skills, "/tmp/work");
    assert!(prompt.contains("<available_skills>"));
    assert!(prompt.contains("<name>brave-search</name>"));
    assert!(prompt.contains("Web search via Brave API"));
    assert!(!prompt.contains("Current date:"));
    assert!(prompt.contains("Current working directory: /tmp/work"));
}

#[test]
fn build_system_prompt_excludes_hidden_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("hidden"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Should not appear".to_owned(),
            file_path: PathBuf::from("/skills/hidden/SKILL.md"),
            add_to_prompt: false,
        },
    );
    let prompt = build_system_prompt(&skills, "/tmp/work");
    assert!(!prompt.contains("<available_skills>"));
    assert!(!prompt.contains("hidden"));
}

#[test]
fn build_system_prompt_escapes_skill_xml_text() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("weird-skill"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Use </description> & <tag> \"quotes\"".to_owned(),
            file_path: PathBuf::from("/skills/weird-skill/SKILL.md"),
            add_to_prompt: true,
        },
    );
    let prompt = build_system_prompt(&skills, "/tmp/work");
    assert!(prompt.contains("Use &lt;/description&gt; &amp; &lt;tag&gt; &quot;quotes&quot;"));
    assert!(!prompt.contains("Use </description>"));
}

#[test]
fn skill_tool_reads_file_content() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let skill_dir = td.path().join("my-skill");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        "---\nname: my-skill\ndescription: A test skill\n---\n# Instructions\nDo the thing.",
    )
    .expect("write");

    let mut h = echo_harness(&sp).expect("start");

    // Manually insert a discovered skill.
    h.discovered_skills.insert(
        tau_proto::SkillName::from("my-skill"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "A test skill".to_owned(),
            file_path: skill_file,
            add_to_prompt: true,
        },
    );

    // Directly invoke the skill tool handler.
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid_for_state, vec!["call-skill".into()]);
    let call = AgentToolCall {
        id: "call-skill".into(),
        name: "skill".into(),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("query".to_owned()),
            CborValue::Text("my-skill".to_owned()),
        )]),
        display: None,
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    // Verify the tool result was persisted.
    let branch = h.store.session("s1").expect("session").current_branch();
    let has_skill_result = branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                outcome: ToolActivityOutcome::Result { .. },
                ..
            })
        )
    });
    assert!(has_skill_result, "expected skill tool result in session");
    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().any(|entry| matches!(
            &entry.event,
            Event::ToolResult(result) if result.call_id.as_str() == "call-skill"
        )),
        "expected skill tool result in durable session event log"
    );
}

#[test]
fn skill_tool_returns_error_for_missing_query() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid_for_state, vec!["call-missing".into()]);
    let call = AgentToolCall {
        id: "call-missing".into(),
        name: "skill".into(),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("action".to_owned()),
            CborValue::Text("load".to_owned()),
        )]),
        display: None,
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    let events = h.store.session_events("s1").expect("session events");
    let err = events
        .iter()
        .find_map(|entry| match &entry.event {
            Event::ToolError(e) if e.call_id.as_str() == "call-missing" => Some(e),
            _ => None,
        })
        .expect("tool error");
    assert!(err.message.contains("missing required argument: query"));
}

#[test]
fn skill_tool_search_matches_name_description_and_optional_content() {
    // Query matching backs progressive skill discovery: when most
    // skills are not advertised at session start, the agent must be
    // able to find them by keyword. Default scope is name +
    // description; `search_content: true` opts into grepping bodies.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    // Use unique tokens that won't collide with the user's real
    // `~/.agents/skills` library that `echo_harness` discovers during
    // eager init.
    const KW: &str = "zqxtoken";
    const BODY_KW: &str = "zqxbody";
    const YAML_ONLY_KW: &str = "zqxyamlonly";

    let alpha_dir = td.path().join("zqx-alpha");
    std::fs::create_dir_all(&alpha_dir).expect("mkdir");
    let alpha_file = alpha_dir.join("SKILL.md");
    std::fs::write(
        &alpha_file,
        format!(
            "---\nname: zqx-alpha\ndescription: {KW} helpers\n---\nalpha body, mentions {BODY_KW}"
        ),
    )
    .expect("write alpha");

    let beta_dir = td.path().join("zqx-beta");
    std::fs::create_dir_all(&beta_dir).expect("mkdir");
    let beta_file = beta_dir.join("SKILL.md");
    std::fs::write(
        &beta_file,
        format!(
            "---\nname: zqx-beta\ndescription: unrelated thing\n---\nbeta body, mentions {BODY_KW} too"
        ),
    )
    .expect("write beta");

    let gamma_dir = td.path().join("zqx-gamma");
    std::fs::create_dir_all(&gamma_dir).expect("mkdir");
    let gamma_file = gamma_dir.join("SKILL.md");
    std::fs::write(
        &gamma_file,
        format!(
            "---\nname: zqx-gamma\ndescription: a different topic\nnotes: {YAML_ONLY_KW}\n---\nno keyword references here"
        ),
    )
    .expect("write gamma");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-alpha"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("{KW} helpers"),
            file_path: alpha_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-beta"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "unrelated thing".to_owned(),
            file_path: beta_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-gamma"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "a different topic".to_owned(),
            file_path: gamma_file,
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    let call_search = |query: &str, search_content: bool, id: &str| AgentToolCall {
        id: id.into(),
        name: "skill".into(),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("query".to_owned()),
                CborValue::Text(query.to_owned()),
            ),
            (
                CborValue::Text("search_content".to_owned()),
                CborValue::Bool(search_content),
            ),
        ]),
        display: None,
    };

    let read_matches = |h: &Harness, call_id: &str| -> Vec<String> {
        let events = h.store.session_events("s1").expect("events");
        let result = events
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                Event::ToolResult(r) if r.call_id.as_str() == call_id => Some(r.result.clone()),
                _ => None,
            })
            .expect("tool result");
        let CborValue::Map(top) = result else {
            panic!("result must be a map")
        };
        let matches = top
            .iter()
            .find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Array(arr)) if k == "matches" => Some(arr.clone()),
                _ => None,
            })
            .expect("matches array");
        matches
            .into_iter()
            .map(|m| {
                let CborValue::Map(entries) = m else {
                    panic!("match must be a map")
                };
                entries
                    .into_iter()
                    .find_map(|(k, v)| match (k, v) {
                        (CborValue::Text(k), CborValue::Text(v)) if k == "name" => Some(v),
                        _ => None,
                    })
                    .expect("name in match")
            })
            .collect()
    };
    let read_display = |h: &Harness, call_id: &str| -> tau_proto::ToolDisplay {
        let events = h.store.session_events("s1").expect("events");
        events
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                Event::ToolResult(r) if r.call_id.as_str() == call_id => r.display.clone(),
                _ => None,
            })
            .expect("tool result display")
    };
    let read_loaded_name = |h: &Harness, call_id: &str| -> String {
        let events = h.store.session_events("s1").expect("events");
        let result = events
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                Event::ToolResult(r) if r.call_id.as_str() == call_id => Some(r.result.clone()),
                _ => None,
            })
            .expect("tool result");
        let CborValue::Map(top) = result else {
            panic!("result must be a map")
        };
        top.into_iter()
            .find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Text(v)) if k == "name" => Some(v),
                _ => None,
            })
            .expect("loaded skill name")
    };

    // Description match: KW only appears in zqx-alpha's description.
    seed_tools_running(&mut h, &cid, vec!["call-1".into()]);
    h.handle_skill_tool_call(&cid, &call_search(KW, false, "call-1"))
        .expect("search 1");
    assert_eq!(read_loaded_name(&h, "call-1"), "zqx-alpha");
    let display = read_display(&h, "call-1");
    assert_eq!(display.stats.lines, Some(1));
    assert!(display.stats.bytes.is_some_and(|bytes| 0 < bytes));

    // Default scope must NOT search content: BODY_KW appears only in
    // alpha and beta bodies. With search_content=false → no hits.
    seed_tools_running(&mut h, &cid, vec!["call-2".into()]);
    h.handle_skill_tool_call(&cid, &call_search(BODY_KW, false, "call-2"))
        .expect("search 2");
    let empty: Vec<String> = Vec::new();
    assert_eq!(read_matches(&h, "call-2"), empty);
    let display = read_display(&h, "call-2");
    assert_eq!(display.stats.matches, Some(0));
    assert_eq!(display.stats.lines, None);
    assert_eq!(display.stats.bytes, None);
    assert_eq!(display.status_text, "ok: no matches");

    // Opt into content search: now alpha and beta both match,
    // sorted alphabetically.
    seed_tools_running(&mut h, &cid, vec!["call-3".into()]);
    h.handle_skill_tool_call(&cid, &call_search(BODY_KW, true, "call-3"))
        .expect("search 3");
    assert_eq!(read_matches(&h, "call-3"), vec!["zqx-alpha", "zqx-beta"]);

    // Frontmatter is stripped before content search.
    seed_tools_running(&mut h, &cid, vec!["call-4".into()]);
    h.handle_skill_tool_call(&cid, &call_search(YAML_ONLY_KW, true, "call-4"))
        .expect("search 4");
    let empty: Vec<String> = Vec::new();
    assert_eq!(read_matches(&h, "call-4"), empty);

    // Name match works case-insensitively and ignores padding.
    seed_tools_running(&mut h, &cid, vec!["call-5".into()]);
    h.handle_skill_tool_call(&cid, &call_search(" ZQX-ALPHA ", false, "call-5"))
        .expect("search 5");
    assert_eq!(read_loaded_name(&h, "call-5"), "zqx-alpha");
}

#[test]
fn skill_tool_search_accepts_multiple_terms_and_ranks_by_hit_count() {
    // Multi-term search: the agent fires several plausible terms at
    // once, the harness scores each skill by how many terms matched
    // it, and returns hits sorted by score descending. Ties break on
    // name to keep the output deterministic.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    const T1: &str = "zqxalpha";
    const T2: &str = "zqxbeta";

    let alpha_dir = td.path().join("zqx-alpha");
    std::fs::create_dir_all(&alpha_dir).expect("mkdir");
    let alpha_file = alpha_dir.join("SKILL.md");
    std::fs::write(
        &alpha_file,
        format!("---\nname: zqx-alpha\ndescription: matches {T1} and {T2}\n---\nbody"),
    )
    .expect("write alpha");

    let beta_dir = td.path().join("zqx-beta");
    std::fs::create_dir_all(&beta_dir).expect("mkdir");
    let beta_file = beta_dir.join("SKILL.md");
    std::fs::write(
        &beta_file,
        format!("---\nname: zqx-beta\ndescription: matches only {T1}\n---\nbody"),
    )
    .expect("write beta");

    let gamma_dir = td.path().join("zqx-gamma");
    std::fs::create_dir_all(&gamma_dir).expect("mkdir");
    let gamma_file = gamma_dir.join("SKILL.md");
    std::fs::write(
        &gamma_file,
        "---\nname: zqx-gamma\ndescription: unrelated\n---\nbody",
    )
    .expect("write gamma");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-alpha"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("matches {T1} and {T2}"),
            file_path: alpha_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-beta"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("matches only {T1}"),
            file_path: beta_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-gamma"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "unrelated".to_owned(),
            file_path: gamma_file,
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    let call_search = |query: &str, id: &str| AgentToolCall {
        id: id.into(),
        name: "skill".into(),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("query".to_owned()),
            CborValue::Text(query.to_owned()),
        )]),
        display: None,
    };

    let read_match_records = |h: &Harness, call_id: &str| -> Vec<(String, u64)> {
        let events = h.store.session_events("s1").expect("events");
        let result = events
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                Event::ToolResult(r) if r.call_id.as_str() == call_id => Some(r.result.clone()),
                _ => None,
            })
            .expect("tool result");
        let CborValue::Map(top) = result else {
            panic!("result must be a map")
        };
        let matches = top
            .iter()
            .find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Array(arr)) if k == "matches" => Some(arr.clone()),
                _ => None,
            })
            .expect("matches array");
        matches
            .into_iter()
            .map(|m| {
                let CborValue::Map(entries) = m else {
                    panic!("match must be a map")
                };
                let mut name = None;
                let mut hits: Option<u64> = None;
                for (k, v) in entries {
                    match (&k, &v) {
                        (CborValue::Text(k), CborValue::Text(v)) if k == "name" => {
                            name = Some(v.clone());
                        }
                        (CborValue::Text(k), CborValue::Integer(i)) if k == "hit_count" => {
                            let n: i128 = (*i).into();
                            hits = Some(n as u64);
                        }
                        _ => {}
                    }
                }
                (name.expect("name"), hits.expect("hit_count"))
            })
            .collect()
    };

    seed_tools_running(&mut h, &cid, vec!["call-multi".into()]);
    h.handle_skill_tool_call(&cid, &call_search(&format!("{T1} {T2}"), "call-multi"))
        .expect("multi search");
    let records = read_match_records(&h, "call-multi");
    assert_eq!(
        records,
        vec![("zqx-alpha".to_owned(), 2), ("zqx-beta".to_owned(), 1),],
        "alpha matches both terms (rank 2), beta matches one (rank 1), \
         gamma matches none and must be filtered out",
    );

    seed_tools_running(&mut h, &cid, vec!["call-dedup".into()]);
    h.handle_skill_tool_call(&cid, &call_search(" zqxalpha  ZQXALPHA ", "call-dedup"))
        .expect("dedup search");
    assert_eq!(
        read_match_records(&h, "call-dedup"),
        vec![("zqx-alpha".to_owned(), 1), ("zqx-beta".to_owned(), 1),],
        "duplicate terms should not inflate hit_count",
    );

    // A single matching skill is loaded directly.
    seed_tools_running(&mut h, &cid, vec!["call-single".into()]);
    h.handle_skill_tool_call(&cid, &call_search(T2, "call-single"))
        .expect("single term");
    let events = h.store.session_events("s1").expect("events");
    let loaded_name = events
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            Event::ToolResult(r) if r.call_id.as_str() == "call-single" => Some(r.result.clone()),
            _ => None,
        })
        .and_then(|result| match result {
            CborValue::Map(entries) => entries.into_iter().find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Text(v)) if k == "name" => Some(v),
                _ => None,
            }),
            _ => None,
        })
        .expect("loaded skill name");
    assert_eq!(loaded_name, "zqx-alpha");

    // Empty query should error rather than silently returning every
    // skill.
    seed_tools_running(&mut h, &cid, vec!["call-empty".into()]);
    h.handle_skill_tool_call(
        &cid,
        &AgentToolCall {
            id: "call-empty".into(),
            name: "skill".into(),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("   ".to_owned()),
            )]),
            display: None,
        },
    )
    .expect("call");
    let events = h.store.session_events("s1").expect("events");
    let saw_error = events.iter().rev().any(|entry| {
        matches!(
            &entry.event,
            Event::ToolError(e) if e.call_id.as_str() == "call-empty"
        )
    });
    assert!(saw_error, "empty query must produce a ToolError");
}

#[test]
fn skill_tool_load_truncates_large_skill_content() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let skill_dir = td.path().join("zqx-large");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    let skill_file = skill_dir.join("SKILL.md");
    let body = "a".repeat(70 * 1024);
    std::fs::write(
        &skill_file,
        format!("---\nname: zqx-large\ndescription: large skill\n---\n{body}"),
    )
    .expect("write skill");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.clear();
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-large"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "large skill".to_owned(),
            file_path: skill_file,
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid, vec!["call-large".into()]);
    h.handle_skill_tool_call(&cid, &skill_call("zqx-large", false, "call-large"))
        .expect("skill call");

    let result = latest_tool_result(&h, "s1", "call-large");
    let content = cbor_text_field(&result, "content").expect("content");
    assert!(content.contains("skill content truncated"));
    assert_eq!(cbor_bool_field(&result, "truncated"), Some(true));
}

#[test]
fn skill_tool_search_caps_match_output() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.clear();

    for i in 0..55 {
        let name = format!("zqx-limit-{i}");
        h.discovered_skills.insert(
            tau_proto::SkillName::from(name.clone()),
            DiscoveredSkill {
                source_id: "skills".into(),
                description: "matches zqxlimit token".to_owned(),
                file_path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
                add_to_prompt: false,
            },
        );
    }

    let cid = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid, vec!["call-limit".into()]);
    h.handle_skill_tool_call(&cid, &skill_call("zqxlimit", false, "call-limit"))
        .expect("skill call");

    let result = latest_tool_result(&h, "s1", "call-limit");
    let matches = cbor_array_field(&result, "matches").expect("matches");
    assert_eq!(matches.len(), 50);
    assert_eq!(cbor_bool_field(&result, "truncated"), Some(true));
    assert_eq!(cbor_u64_field(&result, "total_matches"), Some(55));
}

#[test]
fn skill_tool_loads_exact_single_term_match_even_with_other_hits() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let make_skill = |name: &str, description: &str| {
        let dir = td.path().join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\nbody for {name}"),
        )
        .expect("write skill");
        path
    };
    let exact_path = make_skill("zqxexact", "the exact skill");
    let other_path = make_skill("zqxother", "mentions zqxexact too");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.clear();
    for (name, desc, path) in [
        ("zqxexact", "the exact skill", exact_path),
        ("zqxother", "mentions zqxexact too", other_path),
    ] {
        h.discovered_skills.insert(
            tau_proto::SkillName::from(name),
            DiscoveredSkill {
                source_id: "skills".into(),
                description: desc.to_owned(),
                file_path: path,
                add_to_prompt: false,
            },
        );
    }

    let cid = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid, vec!["call-exact".into()]);
    h.handle_skill_tool_call(
        &cid,
        &AgentToolCall {
            id: "call-exact".into(),
            name: "skill".into(),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("zqxexact".to_owned()),
            )]),
            display: None,
        },
    )
    .expect("skill call");

    let events = h.store.session_events("s1").expect("events");
    let loaded_name = events
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            Event::ToolResult(r) if r.call_id.as_str() == "call-exact" => Some(r.result.clone()),
            _ => None,
        })
        .and_then(|result| match result {
            CborValue::Map(entries) => entries.into_iter().find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Text(v)) if k == "name" => Some(v),
                _ => None,
            }),
            _ => None,
        })
        .expect("loaded skill name");
    assert_eq!(loaded_name, "zqxexact");
}

#[test]
fn skill_tool_default_display_formats_query() {
    let display = super::super::build_tool_args_display(
        "skill",
        &CborValue::Map(vec![
            (
                CborValue::Text("query".to_owned()),
                CborValue::Text(" git  commit git ".to_owned()),
            ),
            (
                CborValue::Text("search_content".to_owned()),
                CborValue::Bool(true),
            ),
        ]),
    )
    .expect("display");

    assert_eq!(display.args, "git commit [content]");
}

#[test]
fn skill_tool_registered_in_tool_list() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let h = echo_harness(&sp).expect("start");
    let defs = h.gather_tool_definitions();
    assert!(
        defs.iter().any(|d| d.name == "skill"),
        "skill tool should be registered; got: {:?}",
        defs.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
}

#[test]
fn extension_skill_invalid_name_is_skipped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.handle_extension_event_inner(
        "source-a",
        Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: tau_proto::SkillName::from("Bad_Name"),
            description: "bad".to_owned(),
            file_path: PathBuf::from("/skills/bad/SKILL.md"),
            add_to_prompt: true,
        }),
    )
    .expect("event");

    assert!(
        !h.discovered_skills
            .contains_key(&tau_proto::SkillName::from("Bad_Name"))
    );
}

#[test]
fn extension_skill_long_description_is_truncated() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let description = "x".repeat(tau_skills::MAX_DESCRIPTION_LENGTH + 16);

    h.handle_extension_event_inner(
        "source-a",
        Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: tau_proto::SkillName::from("long-desc"),
            description,
            file_path: PathBuf::from("/skills/long-desc/SKILL.md"),
            add_to_prompt: true,
        }),
    )
    .expect("event");

    let stored = h
        .discovered_skills
        .get(&tau_proto::SkillName::from("long-desc"))
        .expect("stored");
    assert_eq!(stored.description.len(), tau_skills::MAX_DESCRIPTION_LENGTH);
    assert!(stored.description.ends_with('…'));
}

#[test]
fn duplicate_extension_skill_keeps_first_source_but_allows_refresh() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let skill_event = |description: &str, file_path: &str| {
        Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: tau_proto::SkillName::from("same-skill"),
            description: description.to_owned(),
            file_path: PathBuf::from(file_path),
            add_to_prompt: true,
        })
    };

    h.handle_extension_event_inner("source-a", skill_event("first", "/skills/a/SKILL.md"))
        .expect("first skill");
    h.handle_extension_event_inner("source-a", skill_event("refreshed", "/skills/a2/SKILL.md"))
        .expect("same-source refresh");
    h.handle_extension_event_inner("source-b", skill_event("second", "/skills/b/SKILL.md"))
        .expect("duplicate skill");

    let stored = h
        .discovered_skills
        .get(&tau_proto::SkillName::from("same-skill"))
        .expect("stored skill");
    assert_eq!(stored.source_id, "source-a");
    assert_eq!(stored.description, "refreshed");
    assert_eq!(stored.file_path, PathBuf::from("/skills/a2/SKILL.md"));
}

fn skill_call(query: &str, search_content: bool, id: &str) -> AgentToolCall {
    AgentToolCall {
        id: id.into(),
        name: "skill".into(),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("query".to_owned()),
                CborValue::Text(query.to_owned()),
            ),
            (
                CborValue::Text("search_content".to_owned()),
                CborValue::Bool(search_content),
            ),
        ]),
        display: None,
    }
}

fn latest_tool_result(h: &Harness, session_id: &str, call_id: &str) -> CborValue {
    h.store
        .session_events(session_id)
        .expect("events")
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            Event::ToolResult(r) if r.call_id.as_str() == call_id => Some(r.result.clone()),
            _ => None,
        })
        .expect("tool result")
}

fn cbor_text_field(map: &CborValue, field: &str) -> Option<String> {
    let CborValue::Map(entries) = map else {
        return None;
    };
    entries.iter().find_map(|(k, v)| match (k, v) {
        (CborValue::Text(k), CborValue::Text(v)) if k == field => Some(v.clone()),
        _ => None,
    })
}

fn cbor_bool_field(map: &CborValue, field: &str) -> Option<bool> {
    let CborValue::Map(entries) = map else {
        return None;
    };
    entries.iter().find_map(|(k, v)| match (k, v) {
        (CborValue::Text(k), CborValue::Bool(v)) if k == field => Some(*v),
        _ => None,
    })
}

fn cbor_u64_field(map: &CborValue, field: &str) -> Option<u64> {
    let CborValue::Map(entries) = map else {
        return None;
    };
    entries.iter().find_map(|(k, v)| match (k, v) {
        (CborValue::Text(k), CborValue::Integer(v)) if k == field => {
            let n: i128 = (*v).into();
            u64::try_from(n).ok()
        }
        _ => None,
    })
}

fn cbor_array_field(map: &CborValue, field: &str) -> Option<Vec<CborValue>> {
    let CborValue::Map(entries) = map else {
        return None;
    };
    entries.iter().find_map(|(k, v)| match (k, v) {
        (CborValue::Text(k), CborValue::Array(v)) if k == field => Some(v.clone()),
        _ => None,
    })
}

#[test]
fn gather_tool_definitions_respects_role_tools_profile() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            toolsProfiles: {
                read_only: {
                    shell: false,
                    skill: false,
                },
            },
        }"#,
    )
    .expect("write harness");
    std::fs::write(
        config_dir.join("models.json5"),
        r#"{
            roles: {
                smart: { toolsProfile: "read_only" },
            },
        }"#,
    )
    .expect("write models");

    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir),
        state_dir: Some(state_dir.clone()),
    };
    let h = echo_harness_with_dirs("s1", state_dir, dirs).expect("start");
    let defs = h.gather_tool_definitions();

    assert!(defs.iter().any(|d| d.name == "read"));
    assert!(!defs.iter().any(|d| d.name == "shell"));
    assert!(!defs.iter().any(|d| d.name == "skill"));
}

#[test]
fn aliased_tool_name_is_advertised_and_routed_via_internal_tool() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            toolsProfiles: {
                specialized: {
                    shell: false,
                    test_gpt_shell: true,
                },
            },
        }"#,
    )
    .expect("write harness");
    std::fs::write(
        config_dir.join("models.json5"),
        r#"{
            roles: {
                smart: { toolsProfile: "specialized" },
            },
        }"#,
    )
    .expect("write models");

    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir),
        state_dir: Some(state_dir.clone()),
    };
    let mut h = echo_harness_with_dirs("s1", state_dir, dirs).expect("start");
    let tool_events = connect_test_tool(&mut h, "conn-test-gpt-shell");
    h.registry.register(
        "conn-test-gpt-shell",
        ToolSpec {
            name: ToolName::new("test_gpt_shell"),
            model_visible_name: Some(ToolName::new("shell")),
            description: Some("specialized shell".to_owned()),
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
            enabled_by_default: false,
            side_effects: ToolSideEffects::Pure,
        },
    );

    let defs = h.gather_tool_definitions();
    assert!(
        defs.iter().any(|d| {
            d.name == "test_gpt_shell"
                && d.model_visible_name
                    .as_ref()
                    .is_some_and(|name| name == "shell")
        }),
        "expected aliased tool definition; got: {defs:?}"
    );
    assert!(!defs.iter().any(|d| d.name == "shell"));

    let cid = h.default_conversation_id.clone();
    h.execute_agent_tool_call(
        &cid,
        &AgentToolCall {
            id: "call-alias".into(),
            name: "shell".into(),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            display: None,
        },
    )
    .expect("execute aliased tool call");

    let routed = tool_events.lock().expect("tool events");
    assert!(
        routed.iter().any(|frame| matches!(
            &frame.frame,
            Frame::Event(Event::ToolInvoke(invoke))
                if invoke.call_id.as_str() == "call-alias" && invoke.tool_name == "test_gpt_shell"
        )),
        "expected internal tool invoke; got: {routed:?}"
    );

    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().any(|entry| matches!(
            &entry.event,
            Event::ToolRequest(request)
                if request.call_id.as_str() == "call-alias" && request.tool_name == "shell"
        )),
        "expected persisted visible tool name; got: {events:?}"
    );
}
