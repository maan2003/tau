use super::*;

#[test]
fn build_system_prompt_includes_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("brave-search"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Web search via Brave API".to_owned(),
            source: DiscoveredSkillSource::File(PathBuf::from("/skills/brave-search/SKILL.md")),
            add_to_prompt: true,
        },
    );
    let prompt = build_system_prompt(&skills, &[]);
    assert!(prompt.contains("<available_skills>"));
    assert!(prompt.contains("<name>brave-search</name>"));
    assert!(prompt.contains("<description>Web search via Brave API</description>"));

    assert!(prompt.contains("Web search via Brave API"));
    assert!(!prompt.contains("Current date:"));
    assert!(!prompt.contains("Current working directory: /tmp/work"));
}

#[test]
fn build_system_prompt_excludes_hidden_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("hidden"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Should not appear".to_owned(),
            source: DiscoveredSkillSource::File(PathBuf::from("/skills/hidden/SKILL.md")),
            add_to_prompt: false,
        },
    );
    let prompt = build_system_prompt(&skills, &[]);
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
            source: DiscoveredSkillSource::File(PathBuf::from("/skills/weird-skill/SKILL.md")),
            add_to_prompt: true,
        },
    );
    let prompt = build_system_prompt(&skills, &[]);
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
        "---\nname: my-skill\ndescription: A test skill\n---\n# Instructions\nDo the thing. __TAU_SELF_KNOWLEDGE_VERSION__",
    )
    .expect("write");

    let mut h = echo_harness(&sp).expect("start");

    // Manually insert a discovered skill.
    h.discovered_skills.insert(
        tau_proto::SkillName::from("my-skill"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "A test skill".to_owned(),
            source: DiscoveredSkillSource::File(skill_file),
            add_to_prompt: true,
        },
    );

    // Directly invoke the skill tool handler.
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid_for_state, &[("call-skill", "skill")]);
    let call = AgentToolCall {
        id: "call-skill".into(),
        name: tau_proto::ToolName::new("skill"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("query".to_owned()),
            CborValue::Text("my-skill".to_owned()),
        )]),
        display: None,
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    // Verify the tool result was folded into the item-model transcript.
    let result = latest_tool_result(&h, "s1", "call-skill");
    assert_eq!(
        cbor_text_field(&result, "description").as_deref(),
        Some("A test skill")
    );
    assert_eq!(
        cbor_text_field(&result, "content").as_deref(),
        Some("# Instructions\nDo the thing. __TAU_SELF_KNOWLEDGE_VERSION__")
    );
}

#[test]
fn skill_tool_returns_error_for_missing_query() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid_for_state, &[("call-missing", "skill")]);
    let call = AgentToolCall {
        id: "call-missing".into(),
        name: tau_proto::ToolName::new("skill"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("action".to_owned()),
            CborValue::Text("load".to_owned()),
        )]),
        display: None,
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    let err = latest_tool_error(&h, "s1", "call-missing");
    assert!(err.contains("missing required argument: query"));
}

#[test]
fn skill_tool_rejects_malformed_search_content() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");
    let cid = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid, &[("call-bad-bool", "skill")]);
    h.handle_skill_tool_call(
        &cid,
        &AgentToolCall {
            id: "call-bad-bool".into(),
            name: tau_proto::ToolName::new("skill"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("anything".to_owned()),
                ),
                (
                    CborValue::Text("search_content".to_owned()),
                    CborValue::Text("true".to_owned()),
                ),
            ]),
            display: None,
        },
    )
    .expect("skill call");

    let err = latest_tool_error(&h, "s1", "call-bad-bool");
    assert!(err.contains("search_content must be a boolean"));
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
            source: DiscoveredSkillSource::File(alpha_file),
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-beta"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "unrelated thing".to_owned(),
            source: DiscoveredSkillSource::File(beta_file),
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-gamma"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "a different topic".to_owned(),
            source: DiscoveredSkillSource::File(gamma_file),
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    let call_search = |query: &str, search_content: bool, id: &str| AgentToolCall {
        id: id.into(),
        name: tau_proto::ToolName::new("skill"),
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
    seed_assistant_tool_round(&mut h, &cid, &[("call-1", "skill")]);
    h.handle_skill_tool_call(&cid, &call_search(KW, false, "call-1"))
        .expect("search 1");
    assert_eq!(read_loaded_name(&h, "call-1"), "zqx-alpha");
    let display = read_display(&h, "call-1");
    assert_eq!(display.stats.lines, Some(1));
    assert!(display.stats.bytes.is_some_and(|bytes| 0 < bytes));

    // Default scope must NOT search content: BODY_KW appears only in
    // alpha and beta bodies. With search_content=false → no hits.
    seed_assistant_tool_round(&mut h, &cid, &[("call-2", "skill")]);
    h.handle_skill_tool_call(&cid, &call_search(BODY_KW, false, "call-2"))
        .expect("search 2");
    let empty: Vec<String> = Vec::new();
    assert_eq!(read_matches(&h, "call-2"), empty);
    let display = read_display(&h, "call-2");
    assert_eq!(display.stats.matches, Some(0));
    assert_eq!(display.stats.lines, None);
    assert_eq!(display.stats.bytes, None);
    assert_eq!(display.status_text, "ok");

    // Opt into content search: now alpha and beta both match,
    // sorted alphabetically.
    seed_assistant_tool_round(&mut h, &cid, &[("call-3", "skill")]);
    h.handle_skill_tool_call(&cid, &call_search(BODY_KW, true, "call-3"))
        .expect("search 3");
    assert_eq!(read_matches(&h, "call-3"), vec!["zqx-alpha", "zqx-beta"]);

    // Frontmatter is stripped before content search.
    seed_assistant_tool_round(&mut h, &cid, &[("call-4", "skill")]);
    h.handle_skill_tool_call(&cid, &call_search(YAML_ONLY_KW, true, "call-4"))
        .expect("search 4");
    let empty: Vec<String> = Vec::new();
    assert_eq!(read_matches(&h, "call-4"), empty);

    // Name match works case-insensitively and ignores padding.
    seed_assistant_tool_round(&mut h, &cid, &[("call-5", "skill")]);
    h.handle_skill_tool_call(&cid, &call_search(" ZQX-ALPHA ", false, "call-5"))
        .expect("search 5");
    assert_eq!(read_loaded_name(&h, "call-5"), "zqx-alpha");

    // Trailing punctuation is ignored while the hyphenated name is preserved.
    seed_assistant_tool_round(&mut h, &cid, &[("call-6", "skill")]);
    h.handle_skill_tool_call(&cid, &call_search(" ZQX-ALPHA. ", false, "call-6"))
        .expect("search 6");
    assert_eq!(read_loaded_name(&h, "call-6"), "zqx-alpha");
}

#[test]
fn skill_tool_search_accepts_multiple_terms_and_ranks_by_matched_terms() {
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
            source: DiscoveredSkillSource::File(alpha_file),
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-beta"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("matches only {T1}"),
            source: DiscoveredSkillSource::File(beta_file),
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-gamma"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "unrelated".to_owned(),
            source: DiscoveredSkillSource::File(gamma_file),
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    let call_search = |query: &str, id: &str| AgentToolCall {
        id: id.into(),
        name: tau_proto::ToolName::new("skill"),
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
                let mut matched_terms: Option<u64> = None;
                for (k, v) in entries {
                    match (&k, &v) {
                        (CborValue::Text(k), CborValue::Text(v)) if k == "name" => {
                            name = Some(v.clone());
                        }
                        (CborValue::Text(k), CborValue::Integer(i)) if k == "matched_terms" => {
                            let n: i128 = (*i).into();
                            matched_terms = Some(n as u64);
                        }
                        _ => {}
                    }
                }
                (name.expect("name"), matched_terms.expect("matched_terms"))
            })
            .collect()
    };

    seed_assistant_tool_round(&mut h, &cid, &[("call-multi", "skill")]);
    h.handle_skill_tool_call(&cid, &call_search(&format!("{T1} {T2}"), "call-multi"))
        .expect("multi search");
    let records = read_match_records(&h, "call-multi");
    assert_eq!(
        records,
        vec![("zqx-alpha".to_owned(), 2), ("zqx-beta".to_owned(), 1),],
        "alpha matches both terms (rank 2), beta matches one (rank 1), \
         gamma matches none and must be filtered out",
    );

    seed_assistant_tool_round(&mut h, &cid, &[("call-dedup", "skill")]);
    h.handle_skill_tool_call(&cid, &call_search(" zqxalpha  ZQXALPHA ", "call-dedup"))
        .expect("dedup search");
    assert_eq!(
        read_match_records(&h, "call-dedup"),
        vec![("zqx-alpha".to_owned(), 1), ("zqx-beta".to_owned(), 1),],
        "duplicate terms should not inflate matched_terms",
    );

    // A single matching skill is loaded directly.
    seed_assistant_tool_round(&mut h, &cid, &[("call-single", "skill")]);
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
    seed_assistant_tool_round(&mut h, &cid, &[("call-empty", "skill")]);
    h.handle_skill_tool_call(
        &cid,
        &AgentToolCall {
            id: "call-empty".into(),
            name: tau_proto::ToolName::new("skill"),
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
            source: DiscoveredSkillSource::File(skill_file),
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid, &[("call-large", "skill")]);
    h.handle_skill_tool_call(&cid, &skill_call("zqx-large", false, "call-large"))
        .expect("skill call");

    let result = latest_tool_result(&h, "s1", "call-large");
    let content = cbor_text_field(&result, "content").expect("content");
    assert!(content.contains("skill content truncated"));
    assert_eq!(cbor_bool_field(&result, "truncated"), Some(true));
}

#[test]
fn skill_tool_errors_when_frontmatter_exceeds_read_limit() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let skill_dir = td.path().join("zqx-frontmatter");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    let skill_file = skill_dir.join("SKILL.md");
    let huge_frontmatter_value = "a".repeat(70 * 1024);
    std::fs::write(
        &skill_file,
        format!(
            "---\nname: zqx-frontmatter\ndescription: large frontmatter\nnotes: {huge_frontmatter_value}\n---\nbody"
        ),
    )
    .expect("write skill");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.clear();
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-frontmatter"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "large frontmatter".to_owned(),
            source: DiscoveredSkillSource::File(skill_file),
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid, &[("call-frontmatter", "skill")]);
    h.handle_skill_tool_call(
        &cid,
        &skill_call("zqx-frontmatter", false, "call-frontmatter"),
    )
    .expect("skill call");

    let err = latest_tool_error(&h, "s1", "call-frontmatter");
    assert!(err.contains("frontmatter closing fence"));
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
                source: DiscoveredSkillSource::File(PathBuf::from(format!(
                    "/skills/{name}/SKILL.md"
                ))),
                add_to_prompt: false,
            },
        );
    }

    let cid = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid, &[("call-limit", "skill")]);
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

    let mut h = echo_harness(&sp).expect("start");
    let exact_path = make_skill("zqxexact", "the exact skill");
    let other_path = make_skill("zqxother", "mentions zqxexact too");
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
                source: DiscoveredSkillSource::File(path),
                add_to_prompt: false,
            },
        );
    }

    let cid = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid, &[("call-exact", "skill")]);
    h.handle_skill_tool_call(
        &cid,
        &AgentToolCall {
            id: "call-exact".into(),
            name: tau_proto::ToolName::new("skill"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("zqxexact".to_owned()),
            )]),
            display: None,
        },
    )
    .expect("skill call");

    let loaded_name = cbor_text_field(&latest_tool_result(&h, "s1", "call-exact"), "name")
        .expect("loaded skill name");
    assert_eq!(loaded_name, "zqxexact");
}

#[test]
fn skill_tool_search_result_includes_guidance_and_matched_fields() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.clear();

    for name in ["zqx-guide-a", "zqx-guide-b"] {
        h.discovered_skills.insert(
            tau_proto::SkillName::from(name),
            DiscoveredSkill {
                source_id: "skills".into(),
                description: "mentions zqxguidance".to_owned(),
                source: DiscoveredSkillSource::File(PathBuf::from(format!(
                    "/skills/{name}/SKILL.md"
                ))),
                add_to_prompt: false,
            },
        );
    }

    let cid = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid, &[("call-guidance", "skill")]);
    h.handle_skill_tool_call(&cid, &skill_call("zqxguidance", false, "call-guidance"))
        .expect("skill call");

    let result = latest_tool_result(&h, "s1", "call-guidance");
    let guidance = cbor_text_field(&result, "guidance").expect("guidance");
    assert!(guidance.contains("exact `name`"));
    assert!(guidance.contains("more distinctive term"));
    assert!(guidance.contains("OR semantics"));
    assert!(guidance.contains("not auto-loaded"));

    let matches = cbor_array_field(&result, "matches").expect("matches");
    let Some(CborValue::Map(first)) = matches.first() else {
        panic!("first match must be a map")
    };
    let matched_fields = first
        .iter()
        .find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Array(fields)) if k == "matched_fields" => {
                Some(fields.clone())
            }
            _ => None,
        })
        .expect("matched_fields");
    assert_eq!(
        matched_fields,
        vec![CborValue::Text("description".to_owned())]
    );
}

#[test]
fn skill_tool_default_display_formats_query() {
    let display = super::super::build_tool_args_display(
        "skill",
        &CborValue::Map(vec![
            (
                CborValue::Text("query".to_owned()),
                CborValue::Text(" git,  commit. git ".to_owned()),
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
fn default_tool_display_formats_requested_line_ranges() {
    let read_display = super::super::build_tool_args_display(
        "read",
        &CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text("src/lib.rs".to_owned()),
            ),
            (
                CborValue::Text("start_line".to_owned()),
                CborValue::Integer(2.into()),
            ),
            (
                CborValue::Text("line_count".to_owned()),
                CborValue::Integer(3.into()),
            ),
        ]),
    )
    .expect("read display");
    assert_eq!(read_display.args, "src/lib.rs 2..5");

    let edit_display = super::super::build_tool_args_display(
        "edit",
        &CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text("src/lib.rs".to_owned()),
            ),
            (
                CborValue::Text("edits".to_owned()),
                CborValue::Array(vec![CborValue::Map(vec![
                    (
                        CborValue::Text("oldText".to_owned()),
                        CborValue::Text("old".to_owned()),
                    ),
                    (
                        CborValue::Text("newText".to_owned()),
                        CborValue::Text("new".to_owned()),
                    ),
                    (
                        CborValue::Text("start_line".to_owned()),
                        CborValue::Integer(2.into()),
                    ),
                    (
                        CborValue::Text("line_count".to_owned()),
                        CborValue::Integer(1.into()),
                    ),
                ])]),
            ),
        ]),
    )
    .expect("edit display");
    assert_eq!(edit_display.args, "src/lib.rs 2..3");
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
fn built_in_tau_self_knowledge_skills_are_available_without_file_paths() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    for (name, advertised) in [
        ("tau-self-knowledge", true),
        ("tau-self-knowledge-architecture", false),
        ("tau-self-knowledge-config", false),
        ("tau-self-knowledge-source-code", false),
        ("tau-self-knowledge-community", false),
        ("tau-self-knowledge-debugging", false),
    ] {
        let skill = h
            .discovered_skills
            .get(&tau_proto::SkillName::from(name))
            .expect("built-in skill is seeded at harness startup");
        assert!(matches!(
            skill.source,
            DiscoveredSkillSource::BuiltIn { .. }
        ));
        assert!(skill.source.file_path().is_none());
        assert_eq!(skill.add_to_prompt, advertised, "{name} prompt flag");
    }

    let prompt = build_system_prompt(&h.discovered_skills, &[]);
    assert!(prompt.contains("<name>tau-self-knowledge</name>"));
    assert!(prompt.contains("Use this skill when the user asks about the Tau coding agent"));
    assert!(!prompt.contains("<name>tau-self-knowledge-architecture</name>"));
    assert!(!prompt.contains("<name>tau-self-knowledge-config</name>"));
    assert!(!prompt.contains("<name>tau-self-knowledge-source-code</name>"));
    assert!(!prompt.contains("<name>tau-self-knowledge-community</name>"));
    assert!(!prompt.contains("<name>tau-self-knowledge-debugging</name>"));

    let cid = h.default_conversation_id.clone();
    seed_assistant_tool_round(&mut h, &cid, &[("call-built-in", "skill")]);
    h.handle_skill_tool_call(
        &cid,
        &skill_call("tau-self-knowledge", false, "call-built-in"),
    )
    .expect("built-in skill call");

    let result = latest_tool_result(&h, "s1", "call-built-in");
    assert_eq!(
        cbor_text_field(&result, "name").as_deref(),
        Some("tau-self-knowledge")
    );
    let content = cbor_text_field(&result, "content").expect("content");
    assert!(content.contains("# Tau self-knowledge"));
    assert!(!content.contains("__TAU_SELF_KNOWLEDGE_VERSION__"));
    assert!(content.contains(&format!("Tau version `{}`", env!("CARGO_PKG_VERSION"))));
    assert!(content.contains("tau-self-knowledge-architecture"));
    assert!(content.contains("tau-self-knowledge-config"));
    assert!(content.contains("tau-self-knowledge-source-code"));
    assert!(content.contains("tau-self-knowledge-community"));
    assert!(content.contains("tau-self-knowledge-debugging"));

    seed_assistant_tool_round(&mut h, &cid, &[("call-debugging", "skill")]);
    h.handle_skill_tool_call(
        &cid,
        &skill_call("tau-self-knowledge-debugging", false, "call-debugging"),
    )
    .expect("debugging skill call");
    let debugging = latest_tool_result(&h, "s1", "call-debugging");
    assert_eq!(
        cbor_text_field(&debugging, "name").as_deref(),
        Some("tau-self-knowledge-debugging")
    );
    assert!(
        cbor_text_field(&debugging, "content")
            .expect("debugging content")
            .contains("## Important paths")
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
    assert_eq!(
        stored.source.file_path(),
        Some(Path::new("/skills/a2/SKILL.md"))
    );
}

fn skill_call(query: &str, search_content: bool, id: &str) -> AgentToolCall {
    AgentToolCall {
        id: id.into(),
        name: tau_proto::ToolName::new("skill"),
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
        .session(session_id)
        .expect("session")
        .nodes()
        .iter()
        .rev()
        .find_map(|node| match &node.entry {
            SessionEntry::ToolResults { items } => items
                .iter()
                .find(|item| item.call_id.as_str() == call_id)
                .map(|item| item.output.raw.clone()),
            _ => None,
        })
        .expect("tool result")
}

fn latest_tool_error(h: &Harness, session_id: &str, call_id: &str) -> String {
    h.store
        .session(session_id)
        .expect("session")
        .nodes()
        .iter()
        .rev()
        .find_map(|node| match &node.entry {
            SessionEntry::ToolResults { items } => items.iter().find_map(|item| {
                (item.call_id.as_str() == call_id).then(|| match &item.status {
                    ToolResultStatus::Error { message } => Some(message.clone()),
                    _ => None,
                })?
            }),
            _ => None,
        })
        .expect("tool error")
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

fn cbor_array_field(map: &CborValue, field: &str) -> Option<Vec<CborValue>> {
    let CborValue::Map(entries) = map else {
        return None;
    };
    entries.iter().find_map(|(k, v)| match (k, v) {
        (CborValue::Text(k), CborValue::Array(v)) if k == field => Some(v.clone()),
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
        (CborValue::Text(k), CborValue::Integer(v)) if k == field => (*v).try_into().ok(),
        _ => None,
    })
}

#[test]
fn gather_tool_definitions_respects_role_tool_lists() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            roles: {
                engineer: { disableTools: ["shell", "skill"] },
            },
        }"#,
    )
    .expect("write harness");
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
fn prompt_fragments_include_only_tools_enabled_for_current_role() {
    // Tool prompt fragments ride along with tool registration, but must only be
    // rendered for tools the current role can actually call. Otherwise a hidden
    // or profile-disabled tool could still steer the model via the system prompt.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.registry.register_with_prompt_fragment(
        "conn-prompt-enabled",
        tau_proto::ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("prompt_enabled"),
                model_visible_name: None,
                description: Some("enabled prompt tool".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                enabled_by_default: true,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "prompt_enabled.instructions",
                tau_proto::PromptPriority::new(10),
                "ENABLED TOOL PROMPT",
            )),
        },
    );
    h.registry.register_with_prompt_fragment(
        "conn-prompt-disabled",
        tau_proto::ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("prompt_disabled"),
                model_visible_name: None,
                description: Some("disabled prompt tool".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                enabled_by_default: false,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "prompt_disabled.instructions",
                tau_proto::PromptPriority::new(5),
                "DISABLED TOOL PROMPT",
            )),
        },
    );

    let fragments = h.gather_prompt_fragments();
    let rendered = fragments
        .iter()
        .map(|fragment| fragment.template.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("ENABLED TOOL PROMPT"));
    assert!(!rendered.contains("DISABLED TOOL PROMPT"));
}

#[test]
fn extension_prompt_fragments_are_included_without_enabled_tools() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.extension_prompt_fragments
        .entry(tau_proto::ConnectionId::new("conn-extension"))
        .or_default()
        .insert(
            "extension.context".to_owned(),
            tau_proto::PromptFragment::new(
                "extension.context",
                tau_proto::PromptPriority::new(10),
                "EXTENSION CONTEXT",
            ),
        );

    let fragments = h.gather_prompt_fragments();
    assert!(fragments.iter().any(|f| f.name == "extension.context"));
}

#[test]
fn extension_and_tool_prompt_fragments_sort_together_by_priority_source_name() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.extension_prompt_fragments
        .entry(tau_proto::ConnectionId::new("conn-b"))
        .or_default()
        .insert(
            "b".to_owned(),
            tau_proto::PromptFragment::new("b", tau_proto::PromptPriority::new(10), "B"),
        );
    h.registry.register_with_prompt_fragment(
        "conn-a",
        tau_proto::ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("prompt_enabled_order"),
                model_visible_name: None,
                description: Some("enabled prompt tool".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                enabled_by_default: true,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "z",
                tau_proto::PromptPriority::new(10),
                "A",
            )),
        },
    );
    h.extension_prompt_fragments
        .entry(tau_proto::ConnectionId::new("conn-a"))
        .or_default()
        .insert(
            "a".to_owned(),
            tau_proto::PromptFragment::new("a", tau_proto::PromptPriority::new(10), "AA"),
        );

    let names = h
        .gather_prompt_fragments()
        .into_iter()
        .filter(|f| matches!(f.name.as_str(), "a" | "z" | "b"))
        .map(|f| (f.priority.get(), f.name))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![(10, "a".into()), (10, "z".into()), (10, "b".into())]
    );
}

#[test]
fn aliased_tool_name_is_advertised_and_routed_via_internal_tool() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            roles: {
                engineer: { tools: ["test_gpt_shell"], disableTools: ["shell"] },
            },
        }"#,
    )
    .expect("write harness");
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
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
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
            name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            display: None,
        },
    )
    .expect("execute aliased tool call");

    let routed = tool_events.lock().expect("tool events");
    assert!(
        routed.iter().any(|frame| matches!(
            peel_inner_event(&frame.frame),
            Some(Event::ToolStarted(invoke))
                if invoke.call_id.as_str() == "call-alias" && invoke.tool_name == "test_gpt_shell"
        )),
        "expected internal tool; got: {routed:?}"
    );

    seed_assistant_tool_round(&mut h, &cid, &[("call-alias", "shell")]);
    let session = h.store.session("s1").expect("session");
    assert!(
        session.nodes().iter().any(|node| matches!(
            &node.entry,
            SessionEntry::AssistantResponse { output_items, .. }
                if output_items.iter().any(|item| {
                    matches!(
                        item,
                        ContextItem::ToolCall(call)
                            if call.call_id.as_str() == "call-alias" && call.name == "shell"
                    )
                })
        )),
        "expected assistant transcript to retain the visible tool name"
    );
}
