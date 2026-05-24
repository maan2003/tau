use super::*;

// -- Frontmatter parsing ------------------------------------------------

#[test]
fn parse_frontmatter_basic() {
    let content = "---\nname: my-skill\ndescription: Does things\n---\n# Body\n";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("Does things")
    );
    assert_eq!(body, "# Body\n");
}

#[test]
fn parse_frontmatter_quoted_values() {
    let content = "---\nname: \"my-skill\"\ndescription: 'A quoted description'\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("A quoted description")
    );
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_boolean_field() {
    let content = "---\nname: shown\ndescription: An advertised skill\nadvertise: true\n---\n";
    let (fm, _body) = parse_frontmatter(content);
    assert_eq!(fm.get("advertise").map(String::as_str), Some("true"));
}

#[test]
fn parse_frontmatter_none_when_missing() {
    let content = "# No frontmatter\nJust body content.";
    let (fm, body) = parse_frontmatter(content);
    assert!(fm.is_empty());
    assert_eq!(body, content);
}

#[test]
fn parse_frontmatter_unclosed() {
    let content = "---\nname: broken\nno closing fence";
    let (fm, body) = parse_frontmatter(content);
    assert!(fm.is_empty());
    assert_eq!(body, content);
}

#[test]
fn parse_frontmatter_bom() {
    let content = "\u{feff}---\nname: bom-skill\ndescription: Has BOM\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("bom-skill"));
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_comments_and_blanks() {
    let content = "---\n# comment\n\nname: foo\ndescription: bar\n---\n";
    let (fm, _body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("foo"));
    assert_eq!(fm.get("description").map(String::as_str), Some("bar"));
}

// -- Skill loading from content -----------------------------------------

#[test]
fn load_skill_valid_defaults_to_not_advertised() {
    let content = "---\nname: my-skill\ndescription: Does useful things\n---\n# Instructions";
    let path = Path::new("/skills/my-skill/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.name, "my-skill");
    assert_eq!(skill.description, "Does useful things");
    assert!(
        !skill.add_to_prompt,
        "skills must opt into auto-advertising via `advertise: true`"
    );
    assert!(diags.is_empty());
}

#[test]
fn load_skill_advertise_true_opts_into_prompt() {
    let content = "---\nname: shown\ndescription: visible\nadvertise: true\n---\nBody";
    let path = Path::new("/skills/shown/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert!(skill.add_to_prompt);
}

#[test]
fn load_skill_missing_description() {
    let content = "---\nname: no-desc\n---\n# Body";
    let path = Path::new("/skills/no-desc/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(diags.iter().any(|d| d.kind == DiagnosticKind::Skipped));
}

#[test]
fn load_skill_empty_description() {
    let content = "---\nname: empty\ndescription:\n---\nBody";
    let path = Path::new("/skills/empty/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(diags.iter().any(|d| d.kind == DiagnosticKind::Skipped));
}

#[test]
fn load_skill_truncates_long_description() {
    let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 16);
    let content = format!("---\nname: long-desc\ndescription: {long}\n---\nBody");
    let path = Path::new("/skills/long-desc/SKILL.md");
    let (skill, diags) = load_skill_from_content(&content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.description.len(), MAX_DESCRIPTION_LENGTH);
    assert!(skill.description.ends_with('…'));
    assert!(
        diags
            .iter()
            .any(|d| { d.kind == DiagnosticKind::Warning && d.message.contains("truncating") })
    );
}

#[test]
fn load_skill_advertise_false_or_missing_keeps_default() {
    let content = "---\nname: hidden\ndescription: A hidden skill\nadvertise: false\n---\n";
    let path = Path::new("/skills/hidden/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert!(!skill.add_to_prompt);
}

#[test]
fn load_skill_name_fallback_to_parent_dir() {
    let content = "---\ndescription: Inferred name\n---\n";
    let path = Path::new("/skills/inferred-name/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.name, "inferred-name");
}

#[test]
fn load_skill_name_mismatch_warning() {
    let content = "---\nname: wrong-name\ndescription: Mismatch test\n---\n";
    let path = Path::new("/skills/actual-dir/SKILL.md");
    let (_skill, diags) = load_skill_from_content(content, path);
    assert!(diags.iter().any(|d| d.message.contains("does not match")));
}

#[test]
fn load_skill_invalid_name_chars() {
    let content = "---\nname: Bad_Name\ndescription: Invalid chars\n---\n";
    let path = Path::new("/skills/Bad_Name/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none(), "invalid name should skip the skill");
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("invalid characters"))
    );
}

#[test]
fn load_skill_hyphen_edges_are_skipped() {
    let content = "---\nname: -bad-\ndescription: bad\n---\n";
    let path = Path::new("/skills/-bad-/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("hyphen"))
    );
}

#[test]
fn load_skill_consecutive_hyphens_are_skipped() {
    let content = "---\nname: a--b\ndescription: bad\n---\n";
    let path = Path::new("/skills/a--b/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("consecutive"))
    );
}

#[test]
fn load_skill_empty_name_is_skipped() {
    // No `name:` field and a parent file_name that won't yield one either
    // (root path has no `file_name()`).
    let content = "---\ndescription: nameless\n---\n";
    let path = Path::new("/");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none(), "empty name should skip the skill");
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("name is empty"))
    );
}

#[test]
fn load_skill_advertise_accepts_case_and_one() {
    for value in ["true", "True", "TRUE", "1"] {
        let content =
            format!("---\nname: shown\ndescription: visible\nadvertise: {value}\n---\nBody");
        let path = Path::new("/skills/shown/SKILL.md");
        let (skill, _diags) = load_skill_from_content(&content, path);
        let skill = skill.expect("should load");
        assert!(skill.add_to_prompt, "advertise: {value} should be truthy");
    }
}

#[test]
fn load_skill_advertise_rejects_other_truthy_words() {
    // `yes` / `on` are not in the accepted set — they stay false silently
    // (documented behavior).
    let content = "---\nname: hidden\ndescription: visible\nadvertise: yes\n---\n";
    let path = Path::new("/skills/hidden/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    assert!(!skill.expect("should load").add_to_prompt);
}

#[test]
fn parse_frontmatter_crlf() {
    let content = "---\r\nname: crlf\r\ndescription: Has CRLF\r\n---\r\nBody line";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("crlf"));
    assert_eq!(fm.get("description").map(String::as_str), Some("Has CRLF"));
    assert_eq!(body, "Body line");
}

#[test]
fn parse_frontmatter_unescapes_double_quoted_strings() {
    // serde_yaml_ng (real YAML) handles escapes inside double-quoted
    // scalars; the previous handwritten parser kept the backslashes
    // literal. This pins the new behavior.
    let content = "---\nname: q\ndescription: \"a \\\"quoted\\\" thing\"\n---\n";
    let (fm, _) = parse_frontmatter(content);
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some(r#"a "quoted" thing"#)
    );
}

#[test]
fn parse_frontmatter_multiline_block_scalar() {
    // Block scalars (`>`) fold newlines into a single string. The
    // contract is "stringified scalar", so this round-trips into the
    // map without losing content.
    let content = "---\nname: ml\ndescription: >\n  line one\n  line two\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("line one line two\n")
    );
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_ignores_indented_fence_in_block_scalar() {
    let content = "---\nname: ml\ndescription: |\n  before\n  ---\n  after\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("before\n---\nafter\n")
    );
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_drops_non_scalar_values() {
    // Lists / mappings / null don't fit the BTreeMap<String, String>
    // contract; the parser silently drops them.
    let content = "---\nname: x\ndescription: x\ntags:\n  - a\n  - b\nempty: null\n---\n";
    let (fm, _) = parse_frontmatter(content);
    assert!(fm.contains_key("name"));
    assert!(fm.contains_key("description"));
    assert!(!fm.contains_key("tags"), "lists are dropped");
    assert!(!fm.contains_key("empty"), "null values are dropped");
}

#[test]
fn parse_frontmatter_invalid_yaml_treats_as_no_frontmatter() {
    // Garbage inside the fence shouldn't panic; it should just yield
    // an empty map (and the body still flows through).
    let content = "---\nname: x\n  bad: indent : here\n  more\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert!(fm.is_empty());
    assert_eq!(body, "Body");
}

#[test]
fn load_skill_invalid_yaml_is_skipped_with_parse_diagnostic() {
    let content = "---\nname: x\n  bad: indent : here\n---\nBody";
    let path = Path::new("/skills/broken/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(diags.iter().any(|d| {
        d.kind == DiagnosticKind::Skipped && d.message.contains("YAML failed to parse")
    }));
}

#[test]
fn parse_frontmatter_crlf_mixed_with_multibyte() {
    // Regression for the off-by-one in find_closing_fence with CRLF: any
    // byte-level offset slip would land inside a UTF-8 multibyte char and
    // panic on slice. With correct offsets it just returns the body.
    let content = "---\r\nname: mb\r\ndescription: café ☕\r\n---\r\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("description").map(String::as_str), Some("café ☕"));
    assert_eq!(body, "Body");
}

#[test]
fn root_md_without_name_uses_file_stem() {
    let content = "---\ndescription: A standalone skill\n---\n";
    let path = Path::new("/skills/standalone.md");
    let (skill, diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.name, "standalone");
    assert!(
        diags
            .iter()
            .all(|d| !d.message.contains("does not match parent directory")),
        "standalone file should not be compared with parent dir: {diags:?}"
    );
}

#[test]
fn skill_base_dir_matches_parent() {
    let content = "---\nname: my-skill\ndescription: x\n---\n";
    let path = Path::new("/skills/my-skill/SKILL.md");
    let (skill, _) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.base_dir(), Path::new("/skills/my-skill"));
}

// -- Directory scanning -------------------------------------------------

#[test]
fn discover_skill_md_in_subdir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let skill_dir = tmp.path().join("my-skill");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Test\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("my-skill/SKILL.md"));
}

#[test]
fn discover_root_md_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("z-standalone.md"),
        "---\nname: z-standalone\ndescription: A standalone skill\n---\n",
    )
    .expect("write");
    fs::write(
        tmp.path().join("a-standalone.md"),
        "---\ndescription: A standalone skill\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with("a-standalone.md"));
    assert!(paths[1].ends_with("z-standalone.md"));
}

#[test]
fn discover_skips_dot_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let hidden = tmp.path().join(".hidden");
    fs::create_dir_all(&hidden).expect("mkdir");
    fs::write(
        hidden.join("SKILL.md"),
        "---\nname: hidden\ndescription: Should be skipped\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert!(paths.is_empty());
}

#[test]
fn discover_skips_node_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let nm = tmp.path().join("node_modules").join("some-skill");
    fs::create_dir_all(&nm).expect("mkdir");
    fs::write(
        nm.join("SKILL.md"),
        "---\nname: some-skill\ndescription: Should be skipped\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert!(paths.is_empty());
}

#[test]
fn discover_does_not_recurse_past_skill_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let parent = tmp.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).expect("mkdir");
    fs::write(
        parent.join("SKILL.md"),
        "---\nname: parent\ndescription: Parent skill\n---\n",
    )
    .expect("write");
    fs::write(
        child.join("SKILL.md"),
        "---\nname: child\ndescription: Should not be found\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("parent/SKILL.md"));
}

#[test]
fn discover_nonexistent_dir() {
    let paths = discover_skill_paths(Path::new("/nonexistent/path"));
    assert!(paths.is_empty());
}

// -- Multi-directory loading --------------------------------------------

#[test]
fn load_from_dirs_dedup() {
    let dir1 = tempfile::tempdir().expect("tempdir");
    let dir2 = tempfile::tempdir().expect("tempdir");

    let s1 = dir1.path().join("my-skill");
    fs::create_dir_all(&s1).expect("mkdir");
    fs::write(
        s1.join("SKILL.md"),
        "---\nname: my-skill\ndescription: First\n---\n",
    )
    .expect("write");

    let s2 = dir2.path().join("my-skill");
    fs::create_dir_all(&s2).expect("mkdir");
    fs::write(
        s2.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Second\n---\n",
    )
    .expect("write");

    let result = load_skills_from_dirs(&[dir1.path().to_owned(), dir2.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].description, "First");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::Collision)
    );
}

#[test]
fn load_from_dir_collision_winner_is_path_sorted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for (dir, description) in [("z-skill", "Second"), ("a-skill", "First")] {
        let skill_dir = tmp.path().join(dir);
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: same-name\ndescription: {description}\n---\n"),
        )
        .expect("write");
    }

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].description, "First");
}

#[test]
fn load_from_empty_dirs() {
    let result = load_skills_from_dirs(&[]);
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.is_empty());
}

#[test]
fn built_in_tau_self_knowledge_skills_load_from_embedded_markdown() {
    let skills = built_in_skills();
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "tau-self-knowledge",
            "tau-self-knowledge-architecture",
            "tau-self-knowledge-config",
            "tau-self-knowledge-email",
            "tau-self-knowledge-source-code",
            "tau-self-knowledge-community",
            "tau-self-knowledge-debugging",
        ]
    );

    let skill = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge")
        .expect("built-in tau self-knowledge skill");
    assert_eq!(
        skill.description,
        "Use this skill when the user asks about the Tau coding agent they are running in, including what Tau is, how it works, built-in self-knowledge, configuration, debugging, source code, community links, or where to find Tau-specific help."
    );
    assert!(skill.add_to_prompt);
    assert!(skill.content.contains("# Tau self-knowledge"));
    assert!(!skill.content.contains(SELF_KNOWLEDGE_VERSION_TOKEN));
    assert!(
        skill
            .content
            .contains(&format!("Tau version `{TAU_VERSION}`"))
    );
    assert!(skill.content.contains("tau-self-knowledge-architecture"));
    assert!(skill.content.contains("tau-self-knowledge-config"));
    assert!(skill.content.contains("tau-self-knowledge-email"));
    assert!(skill.content.contains("tau-self-knowledge-source-code"));
    assert!(skill.content.contains("tau-self-knowledge-community"));
    assert!(skill.content.contains("tau-self-knowledge-debugging"));

    let architecture = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-architecture")
        .expect("built-in architecture skill");
    assert!(!architecture.add_to_prompt);
    assert!(architecture.content.contains("# Tau architecture overview"));

    let config = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-config")
        .expect("built-in config skill");
    assert!(!config.add_to_prompt);
    assert!(config.content.contains("tau provider add"));

    let email = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-email")
        .expect("built-in email skill");
    assert!(!email.add_to_prompt);
    assert!(email.content.contains("std-email"));
    assert!(email.content.contains("trusted_authserv_ids"));
    assert!(email.content.contains("Authentication-Results"));

    let source_code = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-source-code")
        .expect("built-in source code skill");
    assert!(!source_code.add_to_prompt);
    assert!(
        source_code
            .content
            .contains("rad:z3ToHcxKefTYxZEoCoDXmddUkK3a4")
    );

    let community = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-community")
        .expect("built-in community skill");
    assert!(!community.add_to_prompt);
    assert!(community.content.contains("GitHub Discussions"));

    let debugging = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-debugging")
        .expect("built-in debugging skill");
    assert!(!debugging.add_to_prompt);
    assert!(debugging.content.contains("## Important paths"));
}

#[test]
fn load_from_scoped_dirs_applies_prompt_default_when_advertise_is_omitted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for (name, advertise) in [
        ("defaulted", ""),
        ("explicit-hidden", "advertise: false\n"),
        ("explicit-shown", "advertise: true\n"),
    ] {
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: x\n{advertise}---\n"),
        )
        .expect("write");
    }

    let result = load_skills_from_skill_dirs(&[SkillDir {
        path: tmp.path().to_owned(),
        add_to_prompt_by_default: true,
    }]);
    let prompt_flag = |name: &str| {
        result
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .map(|skill| skill.add_to_prompt)
    };

    assert_eq!(prompt_flag("defaulted"), Some(true));
    assert_eq!(prompt_flag("explicit-hidden"), Some(false));
    assert_eq!(prompt_flag("explicit-shown"), Some(true));
}

#[test]
fn load_from_dirs_is_sorted_by_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["zebra", "alpha", "mango", "bravo"] {
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: x\n---\n"),
        )
        .expect("write");
    }

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "bravo", "mango", "zebra"]);
}

#[test]
fn discover_follows_symlinked_dirs() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("real-skill");
    fs::create_dir_all(&real).expect("mkdir");
    fs::write(
        real.join("SKILL.md"),
        "---\nname: real-skill\ndescription: real skill\n---\n",
    )
    .expect("write");

    let link = tmp.path().join("link");
    symlink(&real, &link).expect("symlink");

    let paths = discover_skill_paths(&link);
    assert_eq!(paths.len(), 1);
    assert!(paths[0].starts_with(&link));
}

#[test]
fn discover_symlink_cycles_do_not_recurse_forever() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("real");
    let b = real.join("b");
    fs::create_dir_all(&b).expect("mkdir");
    fs::write(
        b.join("SKILL.md"),
        "---\nname: b\ndescription: nested skill\n---\n",
    )
    .expect("write");
    symlink(&real, b.join("cycle")).expect("symlink");

    let link = tmp.path().join("link");
    symlink(&real, &link).expect("symlink");

    let paths = discover_skill_paths(&link);
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("b/SKILL.md"));
}

// -- strip_frontmatter --------------------------------------------------

#[test]
fn strip_frontmatter_returns_body() {
    let content = "---\nname: x\n---\nThe body.";
    assert_eq!(strip_frontmatter(content), "The body.");
}

#[test]
fn strip_frontmatter_no_frontmatter() {
    let content = "Just content.";
    assert_eq!(strip_frontmatter(content), "Just content.");
}

#[test]
fn has_unclosed_frontmatter_detects_missing_closing_fence() {
    assert!(has_unclosed_frontmatter("---\nname: x\n"));
    assert!(has_unclosed_frontmatter("\u{feff}---\r\nname: x\r\n"));
    assert!(!has_unclosed_frontmatter("---\nname: x\n---\nBody"));
    assert!(!has_unclosed_frontmatter("--- not a fence\n"));
    assert!(!has_unclosed_frontmatter("Body only"));
}
