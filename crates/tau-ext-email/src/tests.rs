use std::cell::RefCell;
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::thread;

use super::*;

struct FramePair {
    reader: FrameReader<BufReader<UnixStream>>,
    writer: FrameWriter<BufWriter<UnixStream>>,
}

#[derive(Default)]
struct FakeBackend {
    folders: BTreeMap<String, Vec<BackendFolder>>,
    messages: BTreeMap<(String, String), Vec<BackendMessage>>,
    sent: RefCell<Vec<OutgoingMessage>>,
}

impl FakeBackend {
    fn with_work_mail() -> Self {
        let mut fake = Self::default();
        fake.folders.insert(
            "work".to_owned(),
            vec![
                BackendFolder {
                    name: "INBOX".to_owned(),
                    delimiter: "/".to_owned(),
                    selectable: true,
                },
                BackendFolder {
                    name: "Private".to_owned(),
                    delimiter: "/".to_owned(),
                    selectable: true,
                },
            ],
        );
        fake.messages.insert(
            ("work".to_owned(), "INBOX".to_owned()),
            vec![
                BackendMessage {
                    uid: "1".to_owned(),
                    uidvalidity: "uv1".to_owned(),
                    date: "2026-05-24T00:00:00Z".to_owned(),
                    from: "Mallory <mallory@evil.test>".to_owned(),
                    to: vec!["alice@company.com".to_owned()],
                    cc: Vec::new(),
                    subject: "secret subject".to_owned(),
                    body_text: "secret body".to_owned(),
                    flags: vec!["seen".to_owned()],
                },
                BackendMessage {
                    uid: "2".to_owned(),
                    uidvalidity: "uv1".to_owned(),
                    date: "2026-05-24T00:01:00Z".to_owned(),
                    from: "Teammate <team@company.com>".to_owned(),
                    to: vec!["alice@company.com".to_owned()],
                    cc: Vec::new(),
                    subject: "deploy notes".to_owned(),
                    body_text: "safe body".to_owned(),
                    flags: Vec::new(),
                },
            ],
        );
        fake
    }
}

impl EmailBackend for FakeBackend {
    fn list_folders(&self, account: &str) -> Result<Vec<BackendFolder>, String> {
        Ok(self.folders.get(account).cloned().unwrap_or_default())
    }

    fn list_messages(&self, account: &str, folder: &str) -> Result<Vec<BackendMessage>, String> {
        Ok(self
            .messages
            .get(&(account.to_owned(), folder.to_owned()))
            .cloned()
            .unwrap_or_default())
    }

    fn read_message(
        &self,
        account: &str,
        folder: &str,
        uid: &str,
    ) -> Result<BackendMessage, String> {
        self.messages
            .get(&(account.to_owned(), folder.to_owned()))
            .and_then(|messages| messages.iter().find(|message| message.uid == uid).cloned())
            .ok_or_else(|| "message not found".to_owned())
    }

    fn send_message(&mut self, message: &OutgoingMessage) -> Result<String, String> {
        self.sent.borrow_mut().push(OutgoingMessage {
            account: message.account.clone(),
            from: message.from.clone(),
            to: message.to.clone(),
            cc: message.cc.clone(),
            bcc: message.bcc.clone(),
            subject: message.subject.clone(),
            body_text: message.body_text.clone(),
            reply_to: message.reply_to.clone(),
            in_reply_to: message.in_reply_to.clone(),
        });
        Ok("fake-message-id".to_owned())
    }
}

fn spawn_extension() -> FramePair {
    let (ext_stream, harness_stream) = UnixStream::pair().expect("pair");
    let reader_stream = ext_stream.try_clone().expect("clone");
    thread::spawn(move || {
        run(reader_stream, ext_stream).expect("run");
    });
    FramePair {
        reader: FrameReader::new(BufReader::new(
            harness_stream.try_clone().expect("harness clone"),
        )),
        writer: FrameWriter::new(BufWriter::new(harness_stream)),
    }
}

fn drain_startup(reader: &mut FrameReader<BufReader<UnixStream>>) -> ToolSpec {
    loop {
        match reader.read_frame().expect("read").expect("frame") {
            Frame::Event(Event::ToolRegister(register)) => return register.tool,
            Frame::Message(Message::Ready(_)) => panic!("tool should be registered before ready"),
            _ => {}
        }
    }
}

fn drain_action_schema(reader: &mut FrameReader<BufReader<UnixStream>>) -> ActionSchema {
    loop {
        match reader.read_frame().expect("read").expect("frame") {
            Frame::Event(Event::ActionSchemaPublished(published)) => return published.schema,
            Frame::Message(Message::Ready(_)) => {
                panic!("action schema should be published before ready")
            }
            _ => {}
        }
    }
}

fn cfg() -> EmailExtensionConfig {
    EmailExtensionConfig {
        enable: true,
        accounts: vec![AccountConfig {
            id: "work".to_owned(),
            enable: true,
            display_name: Some("Work".to_owned()),
            from: "Alice <alice@company.com>".to_owned(),
            imap: Some(ImapConfig::default()),
            smtp: Some(SmtpConfig::default()),
            auth: Some(AuthConfig {
                method: Some("password".to_owned()),
                password_env: Some("EMAIL_PASSWORD".to_owned()),
                ..Default::default()
            }),
            folders: FolderPolicy {
                allow: vec!["INBOX".to_owned(), "Archive/*".to_owned()],
                special_sent: None,
            },
        }],
        policy: PolicyConfig {
            incoming_allow: vec!["*@company.com".to_owned()],
            outgoing_allow: vec![
                "bob@company.com".to_owned(),
                "re:.*@trusted\\.test".to_owned(),
            ],
            allow_state_policy_extensions: true,
        },
    }
}

fn engine(temp: &tempfile::TempDir) -> Engine<FakeBackend> {
    Engine {
        config: cfg().validate().expect("valid config"),
        state: StateStore::open(temp.path().join("email-state")).expect("state"),
        backend: FakeBackend::with_work_mail(),
    }
}

fn engine_with_state_policy_extensions(
    temp: &tempfile::TempDir,
    allow_state_policy_extensions: bool,
) -> Engine<FakeBackend> {
    let mut config = cfg();
    config.policy.allow_state_policy_extensions = allow_state_policy_extensions;
    Engine {
        config: config.validate().expect("valid config"),
        state: StateStore::open(temp.path().join("email-state")).expect("state"),
        backend: FakeBackend::with_work_mail(),
    }
}

fn command_args(command: &str, args: Vec<(&str, CborValue)>) -> CborValue {
    cbor_map(vec![
        ("command", CborValue::Text(command.to_owned())),
        ("args", cbor_map(args)),
    ])
}

fn data_field<'a>(value: &'a CborValue, name: &str) -> &'a CborValue {
    let data = map_get(value, "data").expect("data");
    map_get(data, name).expect("field")
}

fn map_get<'a>(value: &'a CborValue, name: &str) -> Option<&'a CborValue> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(key, value)| match key {
        CborValue::Text(key) if key == name => Some(value),
        _ => None,
    })
}

fn text_field(value: &CborValue, name: &str) -> Option<String> {
    match map_get(value, name) {
        Some(CborValue::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

fn array_field<'a>(value: &'a CborValue, name: &str) -> &'a [CborValue] {
    match map_get(value, name).expect("array") {
        CborValue::Array(values) => values,
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn registers_single_email_tool() {
    let mut pair = spawn_extension();
    let tool = drain_startup(&mut pair.reader);
    assert_eq!(tool.name.as_str(), TOOL_NAME);
    assert_eq!(tool.execution_mode, ToolExecutionMode::Exclusive);
    assert!(tool.parameters.is_some());
}

#[test]
fn publishes_email_action_schema_at_startup() {
    let mut pair = spawn_extension();
    let _tool = drain_startup(&mut pair.reader);
    let schema = drain_action_schema(&mut pair.reader);
    schema.validate().expect("email action schema validates");
    assert_eq!(
        schema.executable_action_ids().expect("ids"),
        vec![
            "email.out.list".to_owned(),
            "email.out.open".to_owned(),
            "email.out.approve".to_owned(),
            "email.out.whitelist".to_owned(),
            "email.in.list".to_owned(),
            "email.in.open".to_owned(),
            "email.in.approve".to_owned(),
            "email.in.whitelist".to_owned(),
        ]
    );
    assert_eq!(
        schema
            .parse_line("/email out approve out_0123456789abcdef01234567")
            .expect("parse")
            .action_id,
        "email.out.approve"
    );
}

#[test]
fn disabled_defaults_and_config_validation() {
    let defaults = EmailExtensionConfig::default()
        .validate()
        .expect("default config is safe");
    assert!(!defaults.enable);
    assert!(defaults.accounts.is_empty());
    assert!(defaults.policy.incoming_allow.is_empty());
    assert!(defaults.policy.outgoing_allow.is_empty());
    assert!(defaults.policy.allow_state_policy_extensions);

    let mut config = cfg();
    config.accounts[0].enable = false;
    assert!(!config.validate().expect("valid").accounts["work"].enable);
}

#[test]
fn duplicate_account_ids_and_invalid_regex_are_rejected() {
    let mut dup = cfg();
    dup.accounts.push(dup.accounts[0].clone());
    let duplicate_error = dup.validate().err().expect("duplicate rejected");
    assert!(duplicate_error.contains("duplicate account id"));

    let mut bad_regex = cfg();
    bad_regex.policy.incoming_allow = vec!["re:(".to_owned()];
    let regex_error = bad_regex.validate().err().expect("regex rejected");
    assert!(regex_error.contains("invalid regex"));
}

#[test]
fn exact_glob_regex_address_matching_and_normalization() {
    assert_eq!(
        normalize_address("Alice Example <ALICE@Example.COM>"),
        Some("alice@example.com".to_owned())
    );
    assert!(
        AddressPattern::compile("alice@example.com")
            .expect("exact")
            .matches("Alice <ALICE@EXAMPLE.com>")
    );
    assert!(
        AddressPattern::compile("*@company.com")
            .expect("glob")
            .matches("Team@Company.Com")
    );
    assert!(
        AddressPattern::compile("*@Company.COM")
            .expect("uppercase glob")
            .matches("Team@company.com")
    );
    assert!(
        AddressPattern::compile("re:alerts\\+.*@example\\.org")
            .expect("regex")
            .matches("alerts+deploy@example.org")
    );
    assert!(
        !AddressPattern::compile("bob@example.com")
            .expect("exact")
            .matches("Bob Example <alice@example.com>")
    );
}

#[test]
fn folder_allowlist_behavior() {
    let config = cfg().validate().expect("valid");
    let folders = &config.accounts["work"].folders;
    assert!(folders.allows("INBOX"));
    assert!(folders.allows("Archive/2026"));
    assert!(!folders.allows("Private"));
    assert!(
        !ValidatedFolderPolicy {
            matchers: Vec::new()
        }
        .allows("INBOX")
    );
}

#[test]
fn list_accounts_returns_config_without_secrets_and_folders_are_whitelisted() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let accounts = engine.dispatch(EmailCommand::ListAccounts);
    let CborValue::Array(items) = data_field(&accounts, "accounts") else {
        panic!("accounts array")
    };
    assert_eq!(text_field(&items[0], "id"), Some("work".to_owned()));
    assert!(format!("{accounts:?}").contains("alice@company.com"));
    assert!(!format!("{accounts:?}").contains("EMAIL_PASSWORD"));

    let folders = engine.dispatch(EmailCommand::ListFolders {
        account: "work".to_owned(),
    });
    let names: Vec<_> = array_field(map_get(&folders, "data").expect("data"), "folders")
        .iter()
        .filter_map(|f| text_field(f, "name"))
        .collect();
    assert_eq!(names, vec!["INBOX".to_owned()]);
}

#[test]
fn incoming_list_redacts_untrusted_and_shows_whitelisted_subject() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let result = engine.dispatch(EmailCommand::List {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        limit: 10,
        cursor: None,
    });
    let CborValue::Array(messages) = data_field(&result, "messages") else {
        panic!("messages")
    };

    assert_eq!(text_field(&messages[0], "subject"), None);
    assert!(matches!(
        map_get(&messages[0], "subject"),
        Some(CborValue::Null)
    ));
    assert!(!format!("{:?}", messages[0]).contains("secret subject"));
    assert_eq!(
        text_field(&messages[1], "subject"),
        Some("deploy notes".to_owned())
    );
}

#[test]
fn read_approval_creation_repeat_stability_and_exact_approval() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let first = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "1".to_owned(),
    });
    assert_eq!(cbor_text_field(&first, "status"), Some("approval_required"));
    let id = match data_field(&first, "approval_id") {
        CborValue::Text(id) => id.clone(),
        _ => panic!("id"),
    };
    assert!(!format!("{first:?}").contains("secret body"));
    assert!(!format!("{first:?}").contains("secret subject"));

    let second = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "1".to_owned(),
    });
    assert_eq!(
        data_field(&second, "approval_id"),
        &CborValue::Text(id.clone())
    );

    engine.state.approve_incoming(&id).expect("approve");
    let approved = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "1".to_owned(),
    });
    assert_eq!(cbor_text_field(&approved, "status"), Some("ok"));
    assert!(format!("{approved:?}").contains("secret body"));

    let changed_uidvalidity = BackendMessage {
        uidvalidity: "uv2".to_owned(),
        ..engine
            .backend
            .read_message("work", "INBOX", "1")
            .expect("msg")
    };
    engine.backend.messages.insert(
        ("work".to_owned(), "INBOX".to_owned()),
        vec![changed_uidvalidity],
    );
    let changed = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "1".to_owned(),
    });
    assert_eq!(
        cbor_text_field(&changed, "status"),
        Some("approval_required")
    );
}

#[test]
fn outgoing_whitelisted_sends_and_mixed_recipients_queue_whole_message() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let sent = engine.dispatch(EmailCommand::Send {
        account: Some("work".to_owned()),
        from: Some("Alice <alice@company.com>".to_owned()),
        to: vec!["BOB@company.com".to_owned()],
        cc: vec!["ops@trusted.test".to_owned()],
        bcc: Vec::new(),
        subject: "hi".to_owned(),
        body_text: "body".to_owned(),
        reply_to: None,
        in_reply_to: None,
    });
    assert_eq!(cbor_text_field(&sent, "status"), Some("sent"));
    assert_eq!(engine.backend.sent.borrow().len(), 1);

    let queued = engine.dispatch(EmailCommand::Send {
        account: Some("work".to_owned()),
        from: Some("alice@company.com".to_owned()),
        to: vec![
            "bob@company.com".to_owned(),
            "external@example.net".to_owned(),
        ],
        cc: Vec::new(),
        bcc: vec!["hidden@example.net".to_owned()],
        subject: "proposal".to_owned(),
        body_text: "full body".to_owned(),
        reply_to: None,
        in_reply_to: None,
    });
    assert_eq!(
        cbor_text_field(&queued, "status"),
        Some("approval_required")
    );
    assert_eq!(
        engine.backend.sent.borrow().len(),
        1,
        "queued message must not partially send"
    );
    assert!(
        !format!("{queued:?}").contains("hidden@example.net"),
        "approval-required output must not leak bcc"
    );
}

#[test]
fn outgoing_actions_list_open_approve_and_whitelist_drive_policy() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let queued = engine.dispatch(EmailCommand::Send {
        account: Some("work".to_owned()),
        from: Some("alice@company.com".to_owned()),
        to: vec!["external@example.net".to_owned()],
        cc: Vec::new(),
        bcc: vec!["hidden@example.net".to_owned()],
        subject: "proposal".to_owned(),
        body_text: "full draft body".to_owned(),
        reply_to: None,
        in_reply_to: None,
    });
    let id = match data_field(&queued, "approval_id") {
        CborValue::Text(id) => id.clone(),
        _ => panic!("approval id"),
    };

    let listed = engine
        .dispatch_action("email.out.list", &[])
        .expect("list action");
    assert!(listed.contains(&id));
    assert!(listed.contains("external@example.net"));
    assert!(!listed.contains("hidden@example.net"));
    let opened = engine
        .dispatch_action("email.out.open", &[id.clone()])
        .expect("open action");
    assert!(opened.contains("hidden@example.net"));
    assert!(opened.contains("full draft body"));

    engine
        .dispatch_action("email.out.approve", &[id.clone()])
        .expect("approve action");
    let approved_record = engine
        .state
        .approved_outgoing_by_id(&id)
        .expect("approved record");
    assert_eq!(approved_record.status, "approved");
    assert!(engine.state.pending_outgoing_by_id(&id).is_err());
    let approve_again = engine
        .dispatch_action("email.out.approve", &[id.clone()])
        .expect("approve action is idempotent");
    assert!(approve_again.contains("already approved"));
    let sent = engine.dispatch(EmailCommand::Send {
        account: Some("work".to_owned()),
        from: Some("alice@company.com".to_owned()),
        to: vec!["external@example.net".to_owned()],
        cc: Vec::new(),
        bcc: vec!["hidden@example.net".to_owned()],
        subject: "proposal".to_owned(),
        body_text: "full draft body".to_owned(),
        reply_to: None,
        in_reply_to: None,
    });
    assert_eq!(cbor_text_field(&sent, "status"), Some("sent"));

    engine
        .dispatch_action("email.out.whitelist", &["*@new.test".to_owned()])
        .expect("whitelist action");
    let whitelisted = engine.dispatch(EmailCommand::Send {
        account: Some("work".to_owned()),
        from: Some("alice@company.com".to_owned()),
        to: vec!["person@new.test".to_owned()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "hi".to_owned(),
        body_text: "body".to_owned(),
        reply_to: None,
        in_reply_to: None,
    });
    assert_eq!(cbor_text_field(&whitelisted, "status"), Some("sent"));
}

#[test]
fn incoming_actions_list_open_approve_and_whitelist_drive_policy_without_leaks() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let queued = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "1".to_owned(),
    });
    let id = match data_field(&queued, "approval_id") {
        CborValue::Text(id) => id.clone(),
        _ => panic!("approval id"),
    };

    let listed = engine
        .dispatch_action("email.in.list", &[])
        .expect("list action");
    assert!(listed.contains(&id));
    assert!(listed.contains("mallory@evil.test"));
    assert!(!listed.contains("secret subject"));
    assert!(!listed.contains("secret body"));
    let opened = engine
        .dispatch_action("email.in.open", &[id.clone()])
        .expect("open action");
    assert!(opened.contains("subject_redacted: true"));
    assert!(!opened.contains("secret subject"));
    assert!(!opened.contains("secret body"));

    engine
        .dispatch_action("email.in.approve", &[id.clone()])
        .expect("approve action");
    let approved_record = engine
        .state
        .approved_incoming_by_id(&id)
        .expect("approved record");
    assert_eq!(approved_record.status, "approved");
    assert!(engine.state.pending_incoming_by_id(&id).is_err());
    let approve_again = engine
        .dispatch_action("email.in.approve", &[id.clone()])
        .expect("approve action is idempotent");
    assert!(approve_again.contains("already approved"));
    let approved = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "1".to_owned(),
    });
    assert_eq!(cbor_text_field(&approved, "status"), Some("ok"));
    assert!(format!("{approved:?}").contains("secret body"));

    engine.backend.messages.insert(
        ("work".to_owned(), "INBOX".to_owned()),
        vec![BackendMessage {
            uid: "3".to_owned(),
            uidvalidity: "uv1".to_owned(),
            date: "d".to_owned(),
            from: "friend@new.test".to_owned(),
            to: Vec::new(),
            cc: Vec::new(),
            subject: "visible after whitelist".to_owned(),
            body_text: "friend body".to_owned(),
            flags: Vec::new(),
        }],
    );
    engine
        .dispatch_action("email.in.whitelist", &["*@new.test".to_owned()])
        .expect("whitelist action");
    let read = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "3".to_owned(),
    });
    assert_eq!(cbor_text_field(&read, "status"), Some("ok"));
    assert!(format!("{read:?}").contains("friend body"));
}

#[test]
fn whitelist_actions_reject_when_state_policy_extensions_are_disabled() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine_with_state_policy_extensions(&temp, false);

    let outgoing_error = engine
        .dispatch_action("email.out.whitelist", &["*@new.test".to_owned()])
        .expect_err("outgoing whitelist should be rejected");
    assert!(outgoing_error.contains("state policy extensions are disabled"));
    assert!(
        engine
            .state
            .load_outgoing_allow()
            .expect("out allow")
            .is_empty()
    );

    let incoming_error = engine
        .dispatch_action("email.in.whitelist", &["*@new.test".to_owned()])
        .expect_err("incoming whitelist should be rejected");
    assert!(incoming_error.contains("state policy extensions are disabled"));
    assert!(
        engine
            .state
            .load_incoming_allow()
            .expect("in allow")
            .is_empty()
    );
}

#[test]
fn whitelist_actions_reject_invalid_patterns_without_writing_state() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    for pattern in ["", "re:(", "not-an-address"] {
        assert!(
            engine
                .dispatch_action("email.out.whitelist", &[pattern.to_owned()])
                .is_err(),
            "outgoing pattern {pattern:?} should fail"
        );
        assert!(
            engine
                .dispatch_action("email.in.whitelist", &[pattern.to_owned()])
                .is_err(),
            "incoming pattern {pattern:?} should fail"
        );
    }

    assert!(
        engine
            .state
            .load_outgoing_allow()
            .expect("out allow")
            .is_empty()
    );
    assert!(
        engine
            .state
            .load_incoming_allow()
            .expect("in allow")
            .is_empty()
    );
}

#[test]
fn invalid_email_actions_return_errors() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    assert!(engine.dispatch_action("email.out.nope", &[]).is_err());
    assert!(
        engine
            .dispatch_action(
                "email.out.approve",
                &["in_0123456789abcdef01234567".to_owned()]
            )
            .is_err()
    );
    assert!(
        engine
            .dispatch_action(
                "email.out.open",
                &["out_0123456789abcdef01234567/../../x".to_owned()]
            )
            .is_err()
    );
    assert!(
        engine
            .dispatch_action(
                "email.in.approve",
                &["in_0123456789ABCDEF01234567".to_owned()]
            )
            .is_err()
    );
    assert!(
        engine
            .dispatch_action("email.in.open", &["in_0123456789abcdef01234567".to_owned()])
            .is_err()
    );
}

#[test]
fn runtime_action_invoke_returns_action_error_for_bad_id() {
    let mut runtime = RuntimeState {
        config_state: ConfigState::Rejected {
            reason: "bad config".to_owned(),
        },
    };
    let event = runtime.dispatch_action(ActionInvoke {
        invocation_id: tau_proto::ActionInvocationId::new("invoke-1"),
        session_id: tau_proto::SessionId::new("session-1"),
        extension_name: tau_proto::ExtensionName::new("tau-ext-email"),
        instance_id: tau_proto::ExtensionInstanceId::from(1),
        action_id: "email.in.list".to_owned(),
        raw_line: "/email in list".to_owned(),
        argv: Vec::new(),
        arguments: CborValue::Map(Vec::new()),
    });
    let Event::ActionError(error) = event else {
        panic!("expected action error")
    };
    assert_eq!(error.action_id, "email.in.list");
    assert!(error.message.contains("bad config"));
}

#[test]
fn outgoing_exact_message_approval_matching() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let mk_send =
        |subject: &str, reply_to: Option<&str>, in_reply_to: Option<&str>| EmailCommand::Send {
            account: Some("work".to_owned()),
            from: Some("alice@company.com".to_owned()),
            to: vec!["external@example.net".to_owned()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.to_owned(),
            body_text: "body".to_owned(),
            reply_to: reply_to.map(str::to_owned),
            in_reply_to: in_reply_to.map(str::to_owned),
        };
    let queued = engine.dispatch(mk_send("one", Some("reply@example.net"), Some("<m1>")));
    let id = match data_field(&queued, "approval_id") {
        CborValue::Text(id) => id.clone(),
        _ => panic!("id"),
    };
    let changed_subject = engine.dispatch(mk_send("two", Some("reply@example.net"), Some("<m1>")));
    assert_ne!(
        data_field(&changed_subject, "approval_id"),
        &CborValue::Text(id.clone())
    );
    let changed_reply = engine.dispatch(mk_send("one", Some("other@example.net"), Some("<m1>")));
    assert_ne!(
        data_field(&changed_reply, "approval_id"),
        &CborValue::Text(id.clone())
    );
    let changed_thread = engine.dispatch(mk_send("one", Some("reply@example.net"), Some("<m2>")));
    assert_ne!(
        data_field(&changed_thread, "approval_id"),
        &CborValue::Text(id.clone())
    );

    let approval_path = engine
        .state
        .approval_path("outgoing", "pending", &id)
        .expect("approval path");
    let approval_json = std::fs::read_to_string(approval_path).expect("approval json");
    assert!(approval_json.contains("reply@example.net"));
    assert!(approval_json.contains("<m1>"));

    engine.state.approve_outgoing(&id).expect("approve");
    assert_eq!(
        cbor_text_field(
            &engine.dispatch(mk_send("one", Some("reply@example.net"), Some("<m1>"))),
            "status"
        ),
        Some("sent")
    );
    assert_eq!(
        cbor_text_field(
            &engine.dispatch(mk_send("one", Some("other@example.net"), Some("<m1>"))),
            "status"
        ),
        Some("approval_required")
    );
}

#[test]
fn send_rejects_non_empty_attachments_deliberately() {
    let parsed = parse_command(&command_args(
        "send",
        vec![
            (
                "to",
                CborValue::Array(vec![CborValue::Text("external@example.net".to_owned())]),
            ),
            ("subject", CborValue::Text("hi".to_owned())),
            ("body_text", CborValue::Text("body".to_owned())),
            (
                "attachments",
                CborValue::Array(vec![cbor_map(vec![(
                    "name",
                    CborValue::Text("x.txt".to_owned()),
                )])]),
            ),
        ],
    ));
    let Err(error) = parsed else {
        panic!("non-empty attachments must be rejected")
    };
    assert_eq!(
        cbor_nested_text_field(&error, "error", "code"),
        Some("invalid_input")
    );
}

#[test]
fn approval_ids_reject_path_components_and_wrong_shapes() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state = StateStore::open(temp.path().join("state")).expect("state");
    for id in [
        "",
        "../x",
        "in_../x",
        "in_abc",
        "out_0123456789abcdef01234567",
        "in_0123456789ABCDEF01234567",
    ] {
        assert!(
            state.approve_incoming(id).is_err(),
            "{id} should be rejected"
        );
    }
    assert!(validate_approval_id("in_0123456789abcdef01234567", "in").is_ok());
}

#[test]
fn read_body_and_list_results_report_truncation_metadata() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let long_body = format!("{}tail", "x".repeat(READ_BODY_MAX_BYTES));
    engine.backend.messages.insert(
        ("work".to_owned(), "INBOX".to_owned()),
        vec![
            BackendMessage {
                uid: "10".to_owned(),
                uidvalidity: "uv".to_owned(),
                date: "d".to_owned(),
                from: "team@company.com".to_owned(),
                to: Vec::new(),
                cc: Vec::new(),
                subject: "long".to_owned(),
                body_text: long_body,
                flags: Vec::new(),
            },
            BackendMessage {
                uid: "11".to_owned(),
                uidvalidity: "uv".to_owned(),
                date: "d".to_owned(),
                from: "team@company.com".to_owned(),
                to: Vec::new(),
                cc: Vec::new(),
                subject: "next".to_owned(),
                body_text: "body".to_owned(),
                flags: Vec::new(),
            },
        ],
    );

    let read = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "10".to_owned(),
    });
    assert_eq!(data_field(&read, "body_truncated"), &CborValue::Bool(true));
    assert_eq!(
        data_field(&read, "body_shown_bytes"),
        &CborValue::Integer((READ_BODY_MAX_BYTES as u64).into())
    );

    let listed = engine.dispatch(EmailCommand::List {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        limit: 1,
        cursor: None,
    });
    assert_eq!(data_field(&listed, "truncated"), &CborValue::Bool(true));
    assert_eq!(
        data_field(&listed, "next_cursor"),
        &CborValue::Text("1".to_owned())
    );

    let second_page = engine.dispatch(EmailCommand::List {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        limit: 1,
        cursor: Some("1".to_owned()),
    });
    assert_eq!(
        data_field(&second_page, "truncated"),
        &CborValue::Bool(false)
    );
    assert!(matches!(
        data_field(&second_page, "next_cursor"),
        CborValue::Null
    ));
}

#[test]
fn state_allowlist_load_save_and_policy_extension_disable() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state = StateStore::open(temp.path().join("state")).expect("state");
    state
        .save_incoming_allow_records(&[StatePattern {
            kind: "glob".to_owned(),
            pattern: "*@state.test".to_owned(),
            created_at: "now".to_owned(),
            created_by: "test".to_owned(),
            note: None,
        }])
        .expect("save");
    let patterns = state.load_incoming_allow().expect("load");
    assert!(patterns[0].matches("user@state.test"));

    let mut config = cfg();
    config.policy.incoming_allow.clear();
    config.policy.allow_state_policy_extensions = false;
    let mut engine = Engine {
        config: config.validate().expect("valid"),
        state,
        backend: FakeBackend::default(),
    };
    engine.backend.messages.insert(
        ("work".to_owned(), "INBOX".to_owned()),
        vec![BackendMessage {
            uid: "9".to_owned(),
            uidvalidity: "uv".to_owned(),
            date: "d".to_owned(),
            from: "user@state.test".to_owned(),
            to: Vec::new(),
            cc: Vec::new(),
            subject: "state subject".to_owned(),
            body_text: "state body".to_owned(),
            flags: Vec::new(),
        }],
    );
    let read = engine.dispatch(EmailCommand::Read {
        account: "work".to_owned(),
        folder: "INBOX".to_owned(),
        uid: "9".to_owned(),
    });
    assert_eq!(cbor_text_field(&read, "status"), Some("approval_required"));
}

#[test]
fn spoofed_from_and_policy_errors_do_not_leak_content() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = engine(&temp);
    let spoof = engine.dispatch(EmailCommand::Send {
        account: Some("work".to_owned()),
        from: Some("attacker@example.net".to_owned()),
        to: vec!["bob@company.com".to_owned()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "hi".to_owned(),
        body_text: "body".to_owned(),
        reply_to: None,
        in_reply_to: None,
    });
    assert_eq!(
        cbor_nested_text_field(&spoof, "error", "code"),
        Some("policy_denied")
    );

    let denied = engine.dispatch(EmailCommand::List {
        account: "work".to_owned(),
        folder: "Private".to_owned(),
        limit: 10,
        cursor: None,
    });
    assert_eq!(
        cbor_nested_text_field(&denied, "error", "code"),
        Some("folder_not_allowed")
    );
    assert!(!format!("{denied:?}").contains("secret subject"));
    assert!(!format!("{denied:?}").contains("secret body"));
}

#[test]
fn configure_requires_state_dir_and_rejected_config_is_reported() {
    let mut pair = spawn_extension();
    let _tool = drain_startup(&mut pair.reader);
    pair.writer
        .write_frame(&Frame::Message(Message::Configure(tau_proto::Configure {
            config: CborValue::Map(Vec::new()),
            state_dir: None,
        })))
        .expect("configure");
    pair.writer.flush().expect("flush");
    loop {
        if let Frame::Message(Message::ConfigError(error)) =
            pair.reader.read_frame().expect("read").expect("frame")
        {
            assert!(error.message.contains("state_dir"), "{}", error.message);
            break;
        }
    }
}

#[test]
fn parser_accepts_and_rejects_command_shapes() {
    assert_eq!(
        parse_command(&command_args("list_accounts", vec![])).expect("parse"),
        EmailCommand::ListAccounts
    );
    assert!(
        parse_command(&command_args(
            "list",
            vec![
                ("account", CborValue::Text("work".to_owned())),
                ("folder", CborValue::Text("INBOX".to_owned())),
                ("limit", CborValue::Integer(0.into()))
            ]
        ))
        .is_err()
    );
    assert!(
        parse_command(&command_args(
            "send",
            vec![
                ("to", CborValue::Array(Vec::new())),
                ("subject", CborValue::Text("hi".to_owned())),
                ("body_text", CborValue::Text("body".to_owned()))
            ]
        ))
        .is_err()
    );
}
