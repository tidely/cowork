use yrs::{
    Array, ArrayPrelim, Assoc, ClientID, ID, IndexScope, Map, MapPrelim, StickyIndex, Text,
    TextPrelim, Transact,
    branch::{Branch, BranchPtr},
    types::Attrs,
};

use super::*;

fn participant() -> Uuid {
    Uuid::new_v4()
}

fn target(quote: &str) -> CommentTarget {
    CommentTarget {
        message_id: Uuid::new_v4(),
        range: 3..3 + quote.len(),
        quote: quote.to_owned(),
    }
}

fn attachment(name: &str, creator: Uuid) -> AttachmentRecord {
    AttachmentRecord {
        id: AttachmentId::new(),
        name: name.to_owned(),
        kind: AttachmentKind::Png,
        size: 1234,
        creator,
    }
}

fn ids(draft: &Draft) -> Vec<ItemId> {
    draft.items().into_iter().map(|item| item.id).collect()
}

fn replica_of(draft: &Draft) -> Draft {
    let replica = Draft::new();
    replica.apply_update(&draft.encode_state()).unwrap();
    replica
}

/// Exchanges exactly what each side is missing, the way peers will sync.
fn sync(a: &Draft, b: &Draft) {
    let a_to_b = a.encode_diff(&b.state_vector()).unwrap();
    let b_to_a = b.encode_diff(&a.state_vector()).unwrap();
    b.apply_update(&a_to_b).unwrap();
    a.apply_update(&b_to_a).unwrap();
}

fn raw_counts(draft: &Draft) -> (u32, u32) {
    let txn = draft.doc.transact();
    (draft.order.len(&txn), draft.items.len(&txn))
}

// 1. Creating, listing and removing items

#[test]
fn create_and_list_items_in_order() {
    let draft = Draft::new();
    let alice = participant();
    let bob = participant();

    let first = draft.create_prompt(alice, "first prompt");
    let comment_target = target("quoted");
    let comment = draft.create_comment(bob, comment_target.clone(), "a comment");
    let second = draft.create_prompt(bob, "");

    assert_eq!(
        draft.items(),
        vec![
            DraftItem {
                id: first,
                creator: alice,
                body: "first prompt".into(),
                kind: DraftItemKind::Prompt {
                    attachments: vec![]
                },
            },
            DraftItem {
                id: comment,
                creator: bob,
                body: "a comment".into(),
                kind: DraftItemKind::Comment {
                    target: comment_target
                },
            },
            DraftItem {
                id: second,
                creator: bob,
                body: String::new(),
                kind: DraftItemKind::Prompt {
                    attachments: vec![]
                },
            },
        ]
    );

    let item = draft.item(comment).unwrap();
    assert!(item.is_comment() && !item.is_prompt());
    assert!(draft.item(first).unwrap().is_prompt());
    assert!(draft.contains(second));
    assert_eq!(draft.body(first).as_deref(), Some("first prompt"));
    assert!(!draft.contains(ItemId::new()));
    assert_eq!(draft.body(ItemId::new()), None);

    // Order entries are canonical hyphenated UUID strings keyed the same way in `items`.
    let txn = draft.doc.transact();
    let entry = draft.order.get(&txn, 0).unwrap();
    assert_eq!(entry, Out::Any(Any::from(first.to_string())));
    assert!(draft.items.contains_key(&txn, &first.to_string()));
}

#[test]
fn remove_items_updates_order_and_items() {
    let draft = Draft::new();
    let me = participant();
    let a = draft.create_prompt(me, "a");
    let b = draft.create_comment(me, target("x"), "b");
    let c = draft.create_prompt(me, "c");
    assert_eq!(raw_counts(&draft), (3, 3));

    assert_eq!(draft.remove_items(&[b]), 1);
    assert_eq!(ids(&draft), vec![a, c]);
    assert_eq!(raw_counts(&draft), (2, 2));
    assert!(!draft.contains(b));

    // Unknown ids are ignored, duplicates count once.
    assert_eq!(draft.remove_items(&[]), 0);
    assert_eq!(draft.remove_items(&[ItemId::new(), b]), 0);
    assert_eq!(draft.remove_items(&[c, ItemId::new(), c]), 1);
    assert_eq!(ids(&draft), vec![a]);
    assert_eq!(raw_counts(&draft), (1, 1));

    assert_eq!(draft.remove_items(&[a]), 1);
    assert!(draft.items().is_empty());
    assert_eq!(raw_counts(&draft), (0, 0));
}

// 2. Body editing

#[test]
fn set_body_with_multibyte_text() {
    let draft = Draft::new();
    let id = draft.create_prompt(participant(), "héllo 👋 world");

    let edit = draft.set_body(id, "🎉héllo 👋 world").unwrap();
    assert_eq!(edit.range, 0..0);
    assert_eq!(edit.insert, "🎉");

    // Replace the emoji in the middle; 👋 and 🌍 share their leading bytes.
    let edit = draft.set_body(id, "🎉héllo 🌍 world").unwrap();
    assert_eq!(edit.range, 11..15);
    assert_eq!(edit.insert, "🌍");

    let edit = draft.set_body(id, "🎉héllo 🌍 worldé").unwrap();
    assert_eq!(edit.range, 21..21);
    assert_eq!(edit.insert, "é");

    // "é" -> "è" in the middle.
    let edit = draft.set_body(id, "🎉hèllo 🌍 worldé").unwrap();
    assert_eq!(edit.range, 5..7);
    assert_eq!(edit.insert, "è");

    let edit = draft.set_body(id, "hèllo 🌍 world").unwrap();
    assert_eq!(draft.body(id).as_deref(), Some("hèllo 🌍 world"));
    let mut expected = "🎉hèllo 🌍 worldé".to_owned();
    edit.apply(&mut expected);
    assert_eq!(expected, "hèllo 🌍 world");

    assert_eq!(draft.set_body(id, "hèllo 🌍 world"), None);
    assert_eq!(draft.set_body(ItemId::new(), "anything"), None);

    assert!(draft.set_body(id, "").is_some());
    assert_eq!(draft.body(id).as_deref(), Some(""));
}

#[test]
fn edit_body_validates_ranges() {
    let draft = Draft::new();
    let id = draft.create_prompt(participant(), "héllo👋");
    let edit = |range: std::ops::Range<usize>, insert: &str| TextEdit {
        range,
        insert: insert.to_owned(),
    };

    // Inside "é" (bytes 1..3) and inside "👋" (bytes 6..10).
    assert!(!draft.edit_body(id, &edit(2..2, "x")));
    assert!(!draft.edit_body(id, &edit(1..2, "x")));
    assert!(!draft.edit_body(id, &edit(7..10, "")));
    // Out of range and reversed.
    assert!(!draft.edit_body(id, &edit(10..11, "")));
    assert!(!draft.edit_body(id, &edit(11..11, "x")));
    #[allow(clippy::reversed_empty_ranges)]
    let reversed = edit(3..1, "");
    assert!(!draft.edit_body(id, &reversed));
    assert!(!draft.edit_body(ItemId::new(), &edit(0..0, "x")));
    assert_eq!(draft.body(id).as_deref(), Some("héllo👋"));

    assert!(draft.edit_body(id, &edit(1..3, "e")));
    assert!(draft.edit_body(id, &edit(9..9, "!")));
    assert!(draft.edit_body(id, &edit(5..9, " 🌍")));
    assert!(draft.edit_body(id, &edit(0..0, "")));
    assert_eq!(draft.body(id).as_deref(), Some("hello 🌍!"));
}

// 4. Attachments

#[test]
fn attachments() {
    let draft = Draft::new();
    let me = participant();
    let prompt = draft.create_prompt(me, "see attached");
    let other = draft.create_prompt(me, "");
    let comment = draft.create_comment(me, target("x"), "no files here");

    let first = attachment("screenshot.png", me);
    let second = AttachmentRecord {
        kind: AttachmentKind::Text,
        // Above i32::MAX, so lib0 encodes it as a float.
        size: 5_000_000_000,
        ..attachment("notes.txt", me)
    };
    let third = AttachmentRecord {
        kind: AttachmentKind::Jpeg,
        ..attachment("photo.jpg", participant())
    };

    assert!(draft.add_attachment(prompt, first.clone()));
    assert!(draft.add_attachment(prompt, second.clone()));
    assert!(draft.add_attachment(other, third.clone()));
    assert!(!draft.add_attachment(comment, attachment("nope.png", me)));
    assert!(!draft.add_attachment(ItemId::new(), attachment("nope.png", me)));

    assert_eq!(
        draft.item(prompt).unwrap().kind,
        DraftItemKind::Prompt {
            attachments: vec![first.clone(), second.clone()]
        }
    );
    // Records survive replication intact, including the large size.
    assert_eq!(replica_of(&draft).items(), draft.items());

    let mut expected = vec![first.id, second.id, third.id];
    expected.sort();
    assert_eq!(draft.attachment_ids(), expected);

    assert!(draft.remove_attachment(first.id));
    assert!(!draft.remove_attachment(first.id));
    assert!(!draft.remove_attachment(AttachmentId::new()));
    assert_eq!(
        draft.item(prompt).unwrap().kind,
        DraftItemKind::Prompt {
            attachments: vec![second.clone()]
        }
    );
    assert!(draft.remove_attachment(third.id));
    assert_eq!(draft.attachment_ids(), vec![second.id]);

    // Removing the block drops its attachments from the draft too.
    draft.remove_items(&[prompt]);
    assert!(draft.attachment_ids().is_empty());
}

#[test]
fn is_empty_semantics() {
    let draft = Draft::new();
    let me = participant();

    let blank = draft.create_prompt(me, "  \n\t ");
    assert!(draft.item(blank).unwrap().is_empty());

    let attached = draft.create_prompt(me, "");
    assert!(draft.item(attached).unwrap().is_empty());
    draft.add_attachment(attached, attachment("a.png", me));
    assert!(!draft.item(attached).unwrap().is_empty());

    let text = draft.create_prompt(me, " x ");
    assert!(!draft.item(text).unwrap().is_empty());

    let blank_comment = draft.create_comment(me, target("q"), " ");
    assert!(draft.item(blank_comment).unwrap().is_empty());
    let comment = draft.create_comment(me, target("q"), "hm");
    assert!(!draft.item(comment).unwrap().is_empty());
}

// 5. Replication

#[test]
fn concurrent_appends_converge() {
    let a = Draft::new();
    let alice = participant();
    let bob = participant();
    let base = a.create_prompt(alice, "base");
    let b = replica_of(&a);
    assert_eq!(b.items(), a.items());

    let a1 = a.create_prompt(alice, "a1");
    let a2 = a.create_comment(alice, target("q"), "a2");
    let b1 = b.create_prompt(bob, "b1");
    let b2 = b.create_prompt(bob, "b2");
    sync(&a, &b);

    let order = ids(&a);
    assert_eq!(order, ids(&b));
    assert_eq!(a.items(), b.items());
    assert_eq!(order.len(), 5);
    assert_eq!(order[0], base);
    // Each replica's own appends keep their relative order.
    let pos = |id| order.iter().position(|x| *x == id).unwrap();
    assert!(pos(a1) < pos(a2));
    assert!(pos(b1) < pos(b2));
}

#[test]
fn concurrent_body_edits_merge() {
    let a = Draft::new();
    let id = a.create_prompt(participant(), "hello world");
    let b = replica_of(&a);

    a.set_body(id, "hello brave world").unwrap();
    b.set_body(id, "hello world! 👋").unwrap();
    sync(&a, &b);

    assert_eq!(a.body(id).as_deref(), Some("hello brave world! 👋"));
    assert_eq!(b.body(id), a.body(id));
}

#[test]
fn remove_while_editing_converges() {
    let a = Draft::new();
    let me = participant();
    let keep = a.create_prompt(me, "keep");
    let id = a.create_prompt(me, "doomed");
    let b = replica_of(&a);

    assert_eq!(a.remove_items(&[id]), 1);
    b.set_body(id, "doomed but edited").unwrap();
    b.add_attachment(id, attachment("late.png", me));
    sync(&a, &b);

    assert_eq!(ids(&a), vec![keep]);
    assert_eq!(ids(&b), vec![keep]);
    assert!(!b.contains(id));
    assert!(b.attachment_ids().is_empty());
    assert_eq!(raw_counts(&a), raw_counts(&b));
}

#[test]
fn concurrent_removal_of_same_item() {
    let a = Draft::new();
    let me = participant();
    let id = a.create_prompt(me, "gone");
    let other = a.create_comment(me, target("q"), "stays");
    let b = replica_of(&a);

    assert_eq!(a.remove_items(&[id]), 1);
    assert_eq!(b.remove_items(&[id]), 1);
    sync(&a, &b);

    assert_eq!(ids(&a), vec![other]);
    assert_eq!(a.items(), b.items());
    assert_eq!(raw_counts(&a), (1, 1));
    assert_eq!(raw_counts(&b), (1, 1));
}

#[test]
fn apply_update_is_idempotent_and_commutative() {
    let a = Draft::new();
    let me = participant();
    let first = a.create_prompt(me, "one");
    let b = replica_of(&a);
    b.create_comment(me, target("q"), "two");
    a.set_body(first, "one!").unwrap();

    let from_a = a.encode_state();
    let from_b = b.encode_diff(&a.state_vector()).unwrap();

    let c = Draft::new();
    c.apply_update(&from_a).unwrap();
    c.apply_update(&from_b).unwrap();
    let snapshot = c.items();
    c.apply_update(&from_b).unwrap();
    c.apply_update(&from_a).unwrap();
    assert_eq!(c.items(), snapshot);

    let d = Draft::new();
    d.apply_update(&from_b).unwrap(); // Depends on `from_a`; stays pending until it arrives.
    d.apply_update(&from_a).unwrap();
    assert_eq!(d.items(), snapshot);
    assert_eq!(snapshot.len(), 2);

    // Nothing left to send once in sync.
    let c_state = c.state_vector();
    // The encoding's client order isn't stable, so compare decoded.
    assert_eq!(
        StateVector::decode_v1(&c_state).unwrap(),
        StateVector::decode_v1(&d.state_vector()).unwrap()
    );
    let empty = d.encode_diff(&c_state).unwrap();
    c.apply_update(&empty).unwrap();
    assert_eq!(c.items(), snapshot);
}

#[test]
fn invalid_replication_input_is_an_error() {
    let draft = Draft::new();
    draft.create_prompt(participant(), "x");
    assert!(draft.encode_diff(&[0xff, 0xff, 0xff]).is_err());
    assert!(draft.apply_update(&[0xff, 0xff, 0xff]).is_err());
    assert_eq!(draft.items().len(), 1);
}

// 6. Malformed data

#[test]
fn malformed_items_are_skipped() {
    let draft = Draft::new();
    let me = participant();
    let good = draft.create_prompt(me, "good");

    let dangling = ItemId::new();
    let unknown_kind = ItemId::new();
    let missing_body = ItemId::new();
    let bad_target = ItemId::new();
    let bad_creator = ItemId::new();
    let unordered = ItemId::new();
    let malformed_record = AttachmentId::new();
    {
        let mut txn = draft.doc.transact_mut();
        let txn = &mut txn;

        draft.order.push_back(txn, dangling.to_string());
        draft.order.push_back(txn, "not a uuid");
        draft.order.push_back(txn, 42);
        draft.order.push_back(txn, ArrayPrelim::default());

        let item = draft
            .items
            .insert(txn, unknown_kind.to_string(), MapPrelim::default());
        item.insert(txn, KIND, "poll");
        item.insert(txn, CREATOR, me.to_string());
        item.insert(txn, BODY, TextPrelim::new("?"));
        draft.order.push_back(txn, unknown_kind.to_string());

        let item = draft
            .items
            .insert(txn, missing_body.to_string(), MapPrelim::default());
        item.insert(txn, KIND, KIND_PROMPT);
        item.insert(txn, CREATOR, me.to_string());
        item.insert(txn, ATTACHMENTS, ArrayPrelim::default());
        draft.order.push_back(txn, missing_body.to_string());

        let item = draft
            .items
            .insert(txn, bad_target.to_string(), MapPrelim::default());
        item.insert(txn, KIND, KIND_COMMENT);
        item.insert(txn, CREATOR, me.to_string());
        item.insert(txn, BODY, TextPrelim::new("c"));
        let mut reversed = target("q").to_any();
        if let Any::Map(map) = &mut reversed {
            let map = std::sync::Arc::make_mut(map);
            map.insert("start".into(), Any::from(10));
            map.insert("end".into(), Any::from(2));
        }
        item.insert(txn, TARGET, reversed);
        draft.order.push_back(txn, bad_target.to_string());

        let item = draft
            .items
            .insert(txn, bad_creator.to_string(), MapPrelim::default());
        item.insert(txn, KIND, KIND_PROMPT);
        item.insert(txn, CREATOR, "someone");
        item.insert(txn, BODY, TextPrelim::new(""));
        item.insert(txn, ATTACHMENTS, ArrayPrelim::default());
        draft.order.push_back(txn, bad_creator.to_string());

        // Well-formed, but never ordered.
        let item = draft
            .items
            .insert(txn, unordered.to_string(), MapPrelim::default());
        item.insert(txn, KIND, KIND_PROMPT);
        item.insert(txn, CREATOR, me.to_string());
        item.insert(txn, BODY, TextPrelim::new(""));
        item.insert(txn, ATTACHMENTS, ArrayPrelim::default());

        // A duplicate order entry for the good item.
        draft.order.push_back(txn, good.to_string());

        // A malformed attachment record next to a valid one on the good item.
        let Some(Out::YMap(good_map)) = draft.items.get(txn, &good.to_string()) else {
            panic!("good item missing");
        };
        let Some(Out::YArray(attachments)) = good_map.get(txn, ATTACHMENTS) else {
            panic!("attachments missing");
        };
        let mut bad_record = attachment("bad.gif", me).to_any();
        if let Any::Map(map) = &mut bad_record {
            let map = std::sync::Arc::make_mut(map);
            map.insert(
                "id".into(),
                Any::from(malformed_record.as_uuid().to_string()),
            );
            map.insert("kind".into(), Any::from("gif"));
        }
        attachments.push_back(txn, bad_record);
        attachments.push_back(txn, "junk");
    }
    let valid = attachment("ok.png", me);
    assert!(draft.add_attachment(good, valid.clone()));

    assert_eq!(
        draft.items(),
        vec![DraftItem {
            id: good,
            creator: me,
            body: "good".into(),
            kind: DraftItemKind::Prompt {
                attachments: vec![valid.clone()]
            },
        }]
    );
    for id in [
        dangling,
        unknown_kind,
        missing_body,
        bad_target,
        bad_creator,
        unordered,
    ] {
        assert!(!draft.contains(id));
        assert_eq!(draft.set_body(id, "x"), None);
        assert!(!draft.edit_body(
            id,
            &TextEdit {
                range: 0..0,
                insert: "x".into()
            }
        ));
        assert!(!draft.add_attachment(id, attachment("x.png", me)));
    }

    // Hidden records still count as referenced, and can be removed by id.
    let mut expected = vec![valid.id, malformed_record];
    expected.sort();
    assert_eq!(draft.attachment_ids(), expected);
    assert!(draft.remove_attachment(malformed_record));
    assert_eq!(draft.attachment_ids(), vec![valid.id]);

    // Malformed items can still be removed; the duplicate good entry goes with its item.
    assert_eq!(
        draft.remove_items(&[dangling, unknown_kind, unordered, good]),
        4
    );
    assert!(draft.items().is_empty());

    // The same data replicates and is skipped on the other side too.
    let replica = replica_of(&draft);
    assert!(replica.items().is_empty());
}

#[test]
fn body_with_embed_refuses_edits() {
    let draft = Draft::new();
    let id = draft.create_prompt(participant(), "ab");
    {
        let mut txn = draft.doc.transact_mut();
        let Some(Out::YMap(item)) = draft.items.get(&txn, &id.to_string()) else {
            panic!("item missing");
        };
        let Some(Out::YText(body)) = item.get(&txn, BODY) else {
            panic!("body missing");
        };
        body.insert_embed(&mut txn, 1, Any::from("embed"));
    }
    assert_eq!(draft.body(id).as_deref(), Some("ab"));
    assert_eq!(draft.set_body(id, "abc"), None);
    assert_eq!(draft.body(id).as_deref(), Some("ab"));
}

// 7. Local updates

fn take(draft: &Draft) -> Vec<u8> {
    draft.take_local_update().expect("expected a local update")
}

/// The clients whose inserts `update` carries.
fn update_clients(update: &[u8]) -> Vec<ClientID> {
    // Unlike `state_vector`, this includes clients whose blocks don't start at clock 0.
    let mut clients: Vec<ClientID> = Update::decode_v1(update)
        .unwrap()
        .state_vector_lower()
        .iter()
        .map(|(client, _)| *client)
        .collect();
    clients.sort();
    clients
}

fn assert_converged(drafts: &[&Draft]) {
    let (first, rest) = drafts.split_first().unwrap();
    // The state vector encoding's client order isn't stable, so compare decoded.
    let state = |draft: &Draft| StateVector::decode_v1(&draft.state_vector()).unwrap();
    for draft in rest {
        assert_eq!(draft.items(), first.items());
        assert_eq!(draft.attachment_ids(), first.attachment_ids());
        assert_eq!(state(draft), state(first));
    }
}

#[test]
fn draft_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Draft>();
}

#[test]
fn every_local_mutation_is_reported_once() {
    let draft = Draft::new();
    let replica = Draft::new();
    let me = participant();
    assert_eq!(draft.take_local_update(), None);

    let forward = || {
        let update = take(&draft);
        assert_eq!(draft.take_local_update(), None);
        assert!(
            update_clients(&update)
                .iter()
                .all(|client| *client == draft.doc.client_id())
        );
        replica.apply_update(&update).unwrap();
        assert_eq!(replica.take_local_update(), None);
        assert_converged(&[&draft, &replica]);
    };

    let prompt = draft.create_prompt(me, "hello");
    forward();
    draft.create_comment(me, target("q"), "note");
    forward();
    assert!(draft.set_body(prompt, "hello world").is_some());
    forward();
    assert!(draft.edit_body(
        prompt,
        &TextEdit {
            range: 0..5,
            insert: "bye".into()
        }
    ));
    forward();
    let record = attachment("a.png", me);
    assert!(draft.add_attachment(prompt, record.clone()));
    forward();
    assert!(draft.remove_attachment(record.id));
    forward();
    assert_eq!(draft.remove_items(&[prompt]), 1);
    forward();

    assert_eq!(replica.items().len(), 1);
    replica.validate().unwrap();
}

#[test]
fn applied_updates_are_not_reported() {
    let a = Draft::new();
    let b = Draft::new();
    let alice = participant();
    let bob = participant();

    let id = a.create_prompt(alice, "from a");
    let created = take(&a);
    b.apply_update(&created).unwrap();
    b.apply_update(&created).unwrap();
    b.apply_update(&a.encode_state()).unwrap();
    assert_eq!(b.take_local_update(), None);

    // Out of order: `later` stays pending until `edit` arrives, and neither is reported.
    a.set_body(id, "from a, edited").unwrap();
    let edit = take(&a);
    a.create_prompt(alice, "later");
    let later = take(&a);
    b.apply_update(&later).unwrap();
    assert_eq!(b.take_local_update(), None);
    b.apply_update(&edit).unwrap();
    assert_eq!(b.take_local_update(), None);
    assert_converged(&[&a, &b]);

    // Local changes made on top of remote ones carry only local inserts.
    b.set_body(id, "from b").unwrap();
    b.create_prompt(bob, "b's block");
    let from_b = take(&b);
    assert_eq!(update_clients(&from_b), vec![b.doc.client_id()]);
    a.apply_update(&from_b).unwrap();
    assert_eq!(a.take_local_update(), None);
    assert_converged(&[&a, &b]);
}

#[test]
fn no_op_mutations_report_nothing() {
    let draft = Draft::new();
    let me = participant();
    let prompt = draft.create_prompt(me, "same");
    let _ = take(&draft);

    assert_eq!(draft.set_body(prompt, "same"), None);
    assert_eq!(draft.set_body(ItemId::new(), "x"), None);
    assert_eq!(draft.remove_items(&[ItemId::new()]), 0);
    assert_eq!(draft.remove_items(&[]), 0);
    // Valid but empty.
    assert!(draft.edit_body(
        prompt,
        &TextEdit {
            range: 2..2,
            insert: String::new()
        }
    ));
    assert!(!draft.edit_body(
        prompt,
        &TextEdit {
            range: 3..99,
            insert: "x".into()
        }
    ));
    assert!(!draft.add_attachment(ItemId::new(), attachment("x.png", me)));
    assert!(!draft.remove_attachment(AttachmentId::new()));
    draft.items();
    draft.encode_state();
    draft.validate().unwrap();

    assert_eq!(draft.take_local_update(), None);
}

#[test]
fn local_edits_between_takes_are_merged() {
    let a = Draft::new();
    let b = Draft::new();
    let me = participant();

    let first = a.create_prompt(me, "first");
    let comment = a.create_comment(me, target("q"), "comment");
    a.set_body(first, "first!").unwrap();
    assert!(a.edit_body(
        comment,
        &TextEdit {
            range: 0..0,
            insert: "a ".into()
        }
    ));
    let record = attachment("a.png", me);
    assert!(a.add_attachment(first, record.clone()));
    let doomed = a.create_prompt(me, "doomed");
    assert_eq!(a.remove_items(&[doomed]), 1);

    let update = take(&a);
    assert_eq!(a.take_local_update(), None);
    b.apply_update(&update).unwrap();
    assert_converged(&[&a, &b]);
    assert_eq!(ids(&b), vec![first, comment]);

    // A second batch building on the first.
    a.set_body(first, "first!!").unwrap();
    assert!(a.remove_attachment(record.id));
    let second = a.create_prompt(me, "second");
    a.set_body(second, "second, edited").unwrap();
    b.apply_update(&take(&a)).unwrap();
    assert_converged(&[&a, &b]);
    assert_eq!(b.body(second).as_deref(), Some("second, edited"));
    assert_eq!(b.take_local_update(), None);
    b.validate().unwrap();
}

/// One sync round through the host, the way the app relays updates: each collaborator sends only
/// its `take_local_update` output to the host, which checks it and forwards it to the other
/// collaborators; then the host sends its own local update to everyone.
///
/// Returns the host's update.
fn relay_round(host: &Draft, collaborators: &[(Uuid, &Draft)]) -> Option<Vec<u8>> {
    for (author, replica) in collaborators {
        let Some(update) = replica.take_local_update() else {
            continue;
        };
        let before = host.items();
        host.apply_update(&update).unwrap();
        verify_change(&before, &host.items(), *author).unwrap();
        host.validate().unwrap();
        for (other, peer) in collaborators {
            if other != author {
                peer.apply_update(&update).unwrap();
            }
        }
    }

    let host_update = host.take_local_update();
    if let Some(update) = &host_update {
        assert_eq!(update_clients(update), vec![host.doc.client_id()]);
        for (_, peer) in collaborators {
            peer.apply_update(update).unwrap();
        }
    }
    for (_, peer) in collaborators {
        assert_eq!(peer.take_local_update(), None);
        peer.validate().unwrap();
    }
    host_update
}

#[test]
fn host_relays_between_collaborators() {
    let host = Draft::new();
    let host_id = participant();
    let alice = participant();
    let bob = participant();

    let shared = host.create_prompt(host_id, "hello");
    // Joining collaborators get the full state, which delivers the host's pending change too.
    let a = replica_of(&host);
    let b = replica_of(&host);
    let _ = take(&host);
    let before_round = replica_of(&host);
    let collaborators = [(alice, &a), (bob, &b)];

    // Concurrent typing in the same block and concurrent block creation.
    let insert = |draft: &Draft, at: usize, text: &str| {
        assert!(draft.edit_body(
            shared,
            &TextEdit {
                range: at..at,
                insert: text.into()
            }
        ));
    };
    insert(&a, 5, " from alice");
    insert(&b, 0, "bob: ");
    insert(&host, 5, "!");
    let a_block = a.create_prompt(alice, "alice's block");
    let b_comment = b.create_comment(bob, target("q"), "bob's comment");
    let h_block = host.create_prompt(host_id, "host's block");

    let host_update = relay_round(&host, &collaborators).unwrap();
    assert_converged(&[&host, &a, &b]);
    let body = host.body(shared).unwrap();
    for part in ["bob: ", "hello", " from alice", "!"] {
        assert!(body.contains(part), "{body:?} lacks {part:?}");
    }
    let mut expected = vec![shared, a_block, b_comment, h_block];
    let mut actual = ids(&host);
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected);

    // The host's update holds only the host's own changes.
    before_round.apply_update(&host_update).unwrap();
    assert_eq!(ids(&before_round), vec![shared, h_block]);
    assert_eq!(before_round.body(shared).as_deref(), Some("hello!"));

    // Keystroke-by-keystroke typing at the same spot, with rounds in between.
    let end = host.body(shared).unwrap().len();
    for (index, key) in ["a", "b", "c"].iter().enumerate() {
        insert(&a, end, &key.to_uppercase());
        insert(&b, end, key);
        insert(&host, end, "_");
        if index % 2 == 1 {
            relay_round(&host, &collaborators);
            assert_converged(&[&host, &a, &b]);
        }
    }
    relay_round(&host, &collaborators);
    assert_converged(&[&host, &a, &b]);
    let body = host.body(shared).unwrap();
    assert_eq!(body.len(), end + 9);
    for key in ["A", "b", "C", "_"] {
        assert!(body[end..].contains(key), "{body:?}");
    }
    host.validate().unwrap();
}

// 8. Validation

#[test]
fn validate_accepts_api_documents() {
    let draft = Draft::new();
    draft.validate().unwrap();

    let me = participant();
    let other = participant();
    let prompt = draft.create_prompt(me, "prompt 👋");
    let comment = draft.create_comment(me, target("q"), "");
    let empty = draft.create_prompt(me, "");
    let first = attachment("a.png", me);
    let large = AttachmentRecord {
        size: 5_000_000_000,
        ..attachment("b.txt", other)
    };
    assert!(draft.add_attachment(prompt, first.clone()));
    assert!(draft.add_attachment(empty, large));
    draft.set_body(prompt, "prompt, edited 👋").unwrap();
    draft.set_body(comment, "now with text").unwrap();
    draft.validate().unwrap();

    let replica = replica_of(&draft);
    replica.validate().unwrap();
    replica.create_prompt(other, "from the replica");
    replica.set_body(prompt, "edited remotely").unwrap();
    sync(&draft, &replica);
    draft.validate().unwrap();
    replica.validate().unwrap();

    assert!(draft.remove_attachment(first.id));
    draft.remove_items(&[prompt, comment]);
    sync(&draft, &replica);
    draft.validate().unwrap();
    replica.validate().unwrap();

    replica.remove_items(&ids(&replica));
    sync(&draft, &replica);
    assert!(draft.items().is_empty());
    draft.validate().unwrap();
    replica.validate().unwrap();
}

struct Fixture {
    prompt: ItemId,
    prompt_map: MapRef,
    other_prompt_map: MapRef,
    comment_map: MapRef,
    record: AttachmentRecord,
}

fn item_map<T: ReadTxn>(draft: &Draft, txn: &T, id: ItemId) -> MapRef {
    match draft.items.get(txn, &id.to_string()) {
        Some(Out::YMap(map)) => map,
        other => panic!("item {id} is {other:?}"),
    }
}

fn body_of<T: ReadTxn>(txn: &T, item: &MapRef) -> TextRef {
    match item.get(txn, BODY) {
        Some(Out::YText(text)) => text,
        other => panic!("body is {other:?}"),
    }
}

fn attachments_of<T: ReadTxn>(txn: &T, item: &MapRef) -> ArrayRef {
    match item.get(txn, ATTACHMENTS) {
        Some(Out::YArray(array)) => array,
        other => panic!("attachments is {other:?}"),
    }
}

/// The map half of any shared type, to write entries its typed API hides.
fn map_view(branch: &Branch) -> MapRef {
    MapRef::from(BranchPtr::from(branch))
}

/// The sequence half of any shared type, to write content its typed API hides.
fn array_view(branch: &Branch) -> ArrayRef {
    ArrayRef::from(BranchPtr::from(branch))
}

fn with_field(any: Any, key: &str, value: impl Into<Any>) -> Any {
    let Any::Map(mut map) = any else {
        panic!("not a map");
    };
    std::sync::Arc::make_mut(&mut map).insert(key.into(), value.into());
    Any::Map(map)
}

/// Builds a valid draft, breaks it with `corrupt` and checks that `validate` reports an error
/// mentioning `expected`.
fn assert_rejected(expected: &str, corrupt: impl FnOnce(&Draft, &mut TransactionMut, &Fixture)) {
    let draft = Draft::new();
    let me = participant();
    let prompt = draft.create_prompt(me, "prompt");
    let other_prompt = draft.create_prompt(me, "other");
    let comment = draft.create_comment(me, target("q"), "comment");
    let record = attachment("a.png", me);
    assert!(draft.add_attachment(prompt, record.clone()));
    draft.validate().unwrap();

    {
        let mut txn = draft.doc.transact_mut();
        let fixture = Fixture {
            prompt,
            prompt_map: item_map(&draft, &txn, prompt),
            other_prompt_map: item_map(&draft, &txn, other_prompt),
            comment_map: item_map(&draft, &txn, comment),
            record,
        };
        corrupt(&draft, &mut txn, &fixture);
    }

    let error = format!("{:#}", draft.validate().unwrap_err());
    assert!(
        error.contains(expected),
        "expected {expected:?} in {error:?}"
    );
    // The violation replicates, and so does its detection.
    let replica = replica_of(&draft);
    assert!(replica.validate().is_err());
}

#[test]
fn validate_rejects_bad_structure() {
    assert_rejected("not an item id", |d, txn, _| {
        d.order.push_back(txn, 42);
    });
    assert_rejected("not an item id", |d, txn, _| {
        d.order.push_back(txn, "not a uuid");
    });
    assert_rejected("not an item id", |d, txn, _| {
        d.order.push_back(txn, ArrayPrelim::default());
    });
    assert_rejected("more than once", |d, txn, f| {
        d.order.push_back(txn, f.prompt.to_string());
    });
    assert_rejected("has no item", |d, txn, _| {
        d.order.push_back(txn, ItemId::new().to_string());
    });
    assert_rejected("not in order", |d, txn, _| {
        let item = d
            .items
            .insert(txn, ItemId::new().to_string(), MapPrelim::default());
        item.insert(txn, KIND, KIND_COMMENT);
    });
    // Only the canonical id string can be looked up from `order`.
    assert_rejected("not in order", |d, txn, f| {
        let key = f.prompt.as_uuid().simple().to_string();
        d.items.insert(txn, key, MapPrelim::default());
    });
    assert_rejected("not a map", |d, txn, _| {
        let id = ItemId::new().to_string();
        d.items.insert(txn, id.as_str(), "item");
        d.order.push_back(txn, id);
    });
    assert_rejected("`order` root has map entries", |d, txn, _| {
        map_view(d.order.as_ref()).insert(txn, "hidden", 1);
    });
    assert_rejected("`items` root has sequence content", |d, txn, _| {
        array_view(d.items.as_ref()).push_back(txn, 1);
    });
}

#[test]
fn validate_rejects_bad_items() {
    assert_rejected("unsupported kind", |_, txn, f| {
        f.prompt_map.insert(txn, KIND, "poll");
    });
    assert_rejected("kind is missing or not a string", |_, txn, f| {
        f.prompt_map.insert(txn, KIND, 1);
    });
    assert_rejected("kind is missing or not a string", |_, txn, f| {
        f.comment_map.remove(txn, KIND);
    });
    assert_rejected("has fields", |_, txn, f| {
        f.prompt_map.remove(txn, BODY);
    });
    assert_rejected("has fields", |_, txn, f| {
        f.prompt_map.insert(txn, "extra", true);
    });
    assert_rejected("has fields", |_, txn, f| {
        f.prompt_map.insert(txn, TARGET, target("q").to_any());
    });
    assert_rejected("has fields", |_, txn, f| {
        f.comment_map
            .insert(txn, ATTACHMENTS, ArrayPrelim::default());
    });
    assert_rejected("has sequence content", |_, txn, f| {
        array_view(f.comment_map.as_ref()).push_back(txn, 1);
    });
    assert_rejected("is not a UUID", |_, txn, f| {
        f.prompt_map.insert(txn, CREATOR, "someone");
    });
    assert_rejected("creator is not a string", |_, txn, f| {
        f.comment_map.insert(txn, CREATOR, 7);
    });
}

#[test]
fn validate_rejects_bad_bodies() {
    assert_rejected("body is not a text", |_, txn, f| {
        f.prompt_map.insert(txn, BODY, "plain string");
    });
    assert_rejected("body is not a text", |_, txn, f| {
        f.comment_map.insert(txn, BODY, ArrayPrelim::default());
    });
    assert_rejected("body contains embeds", |_, txn, f| {
        body_of(txn, &f.prompt_map).insert_embed(txn, 1, Any::from("embed"));
    });
    assert_rejected("body contains embeds", |_, txn, f| {
        body_of(txn, &f.comment_map).insert_embed(txn, 0, MapPrelim::default());
    });
    assert_rejected("body has formatting attributes", |_, txn, f| {
        let bold = Attrs::from([("bold".into(), true.into())]);
        body_of(txn, &f.prompt_map).format(txn, 0, 3, bold);
    });
    assert_rejected("body has formatting attributes", |_, txn, f| {
        let italic = Attrs::from([("italic".into(), true.into())]);
        body_of(txn, &f.comment_map).insert_with_attributes(txn, 0, "x", italic);
    });
    assert_rejected("body has map entries", |_, txn, f| {
        let body = body_of(txn, &f.prompt_map);
        map_view(body.as_ref()).insert(txn, "hidden", 1);
    });
}

#[test]
fn validate_rejects_bad_attachments_and_targets() {
    assert_rejected("attachments is not an array", |_, txn, f| {
        f.prompt_map.insert(txn, ATTACHMENTS, "none");
    });
    assert_rejected("attachments has map entries", |_, txn, f| {
        let attachments = attachments_of(txn, &f.prompt_map);
        map_view(attachments.as_ref()).insert(txn, "hidden", 1);
    });
    assert_rejected("not an atomic value", |_, txn, f| {
        attachments_of(txn, &f.prompt_map).push_back(txn, MapPrelim::default());
    });
    assert_rejected("exactly the fields", |_, txn, f| {
        attachments_of(txn, &f.prompt_map).push_back(txn, "junk");
    });
    assert_rejected("exactly the fields", |_, txn, f| {
        let record = with_field(attachment("x.png", participant()).to_any(), "extra", 1);
        attachments_of(txn, &f.other_prompt_map).push_back(txn, record);
    });
    assert_rejected("is malformed", |_, txn, f| {
        let record = with_field(attachment("x.gif", participant()).to_any(), "kind", "gif");
        attachments_of(txn, &f.prompt_map).push_back(txn, record);
    });
    assert_rejected("is malformed", |_, txn, f| {
        let record = with_field(attachment("x.png", participant()).to_any(), "size", -1);
        attachments_of(txn, &f.prompt_map).push_back(txn, record);
    });
    assert_rejected("more than once", |_, txn, f| {
        attachments_of(txn, &f.prompt_map).push_back(txn, f.record.to_any());
    });
    assert_rejected("more than once", |_, txn, f| {
        let copy = AttachmentRecord {
            name: "copy.png".into(),
            ..f.record.clone()
        };
        attachments_of(txn, &f.other_prompt_map).push_back(txn, copy.to_any());
    });

    assert_rejected("target is not an atomic value", |_, txn, f| {
        f.comment_map.insert(txn, TARGET, TextPrelim::new("target"));
    });
    assert_rejected("target does not have exactly the fields", |_, txn, f| {
        f.comment_map
            .insert(txn, TARGET, with_field(target("q").to_any(), "extra", 1));
    });
    assert_rejected("target is malformed", |_, txn, f| {
        let reversed = with_field(target("q").to_any(), "start", 100);
        f.comment_map.insert(txn, TARGET, reversed);
    });
    assert_rejected("target is malformed", |_, txn, f| {
        let bad = with_field(target("q").to_any(), "message_id", "not a uuid");
        f.comment_map.insert(txn, TARGET, bad);
    });
}

#[test]
fn validate_rejects_extra_roots() {
    let draft = Draft::new();
    draft.create_prompt(participant(), "fine");

    let foreign = Doc::new();
    let extra = foreign.get_or_insert_map("extra");
    extra.insert(&mut foreign.transact_mut(), "key", "value");
    let update = foreign
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    draft.apply_update(&update).unwrap();

    assert_eq!(draft.items().len(), 1);
    let error = draft.validate().unwrap_err().to_string();
    assert!(error.contains("unexpected root \"extra\""), "{error}");
}

// 9. Change verification

/// Runs `change` on `draft` and verifies it as made by `author`.
fn change_by(draft: &Draft, author: Uuid, change: impl FnOnce()) -> Result<()> {
    let before = draft.items();
    change();
    verify_change(&before, &draft.items(), author)
}

#[test]
fn verify_change_accepts_allowed_changes() {
    let draft = Draft::new();
    let alice = participant();
    let bob = participant();
    let prompt = draft.create_prompt(alice, "alice's");
    let comment = draft.create_comment(alice, target("q"), "alice's comment");
    let record = attachment("a.png", alice);
    assert!(draft.add_attachment(prompt, record.clone()));

    let unchanged = draft.items();
    verify_change(&unchanged, &unchanged, bob).unwrap();

    change_by(&draft, bob, || {
        let own = draft.create_prompt(bob, "bob's");
        assert!(draft.add_attachment(own, attachment("b.png", bob)));
        draft.create_comment(bob, target("r"), "bob's comment");
    })
    .unwrap();
    change_by(&draft, bob, || {
        assert!(draft.add_attachment(prompt, attachment("c.png", bob)));
    })
    .unwrap();
    change_by(&draft, bob, || {
        draft.set_body(prompt, "edited by bob").unwrap();
        draft.set_body(comment, "also edited by bob").unwrap();
    })
    .unwrap();
    change_by(&draft, bob, || {
        assert!(draft.remove_attachment(record.id));
    })
    .unwrap();
    change_by(&draft, bob, || {
        assert_eq!(draft.remove_items(&[prompt, comment]), 2);
    })
    .unwrap();
}

#[test]
fn verify_change_rejects_forbidden_changes() {
    let draft = Draft::new();
    let alice = participant();
    let bob = participant();
    let prompt = draft.create_prompt(alice, "alice's");
    draft.create_comment(alice, target("q"), "alice's comment");
    assert!(draft.add_attachment(prompt, attachment("a.png", alice)));
    let before = draft.items();

    let rejected = |after: &[DraftItem], expected: &str| {
        let error = verify_change(&before, after, bob).unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "expected {expected:?} in {error:?}"
        );
    };

    // Through the API, as the host would see a forged update.
    let replica = replica_of(&draft);
    let error = change_by(&replica, bob, || {
        replica.create_prompt(alice, "forged");
    })
    .unwrap_err();
    assert!(error.to_string().contains("new item"), "{error}");
    let error = change_by(&replica, bob, || {
        let own = replica.create_prompt(bob, "own");
        assert!(replica.add_attachment(own, attachment("forged.png", alice)));
    })
    .unwrap_err();
    assert!(error.to_string().contains("new attachment"), "{error}");

    let mut after = before.clone();
    after.push(DraftItem {
        id: ItemId::new(),
        creator: alice,
        body: String::new(),
        kind: DraftItemKind::Comment {
            target: target("x"),
        },
    });
    rejected(&after, "new item");

    let mut after = before.clone();
    after[0].creator = bob;
    rejected(&after, "changed creator");

    let mut after = before.clone();
    after[0].kind = DraftItemKind::Comment {
        target: target("q"),
    };
    rejected(&after, "changed kind");

    let mut after = before.clone();
    after[1].kind = DraftItemKind::Prompt {
        attachments: Vec::new(),
    };
    rejected(&after, "changed kind");

    let mut after = before.clone();
    after[1].kind = DraftItemKind::Comment {
        target: target("another quote"),
    };
    rejected(&after, "changed target");

    fn attachments_of(items: &mut [DraftItem]) -> &mut Vec<AttachmentRecord> {
        match &mut items[0].kind {
            DraftItemKind::Prompt { attachments } => attachments,
            DraftItemKind::Comment { .. } => panic!("not a prompt"),
        }
    }

    let mut after = before.clone();
    attachments_of(&mut after).push(attachment("forged.png", alice));
    rejected(&after, "new attachment");

    let mut after = before.clone();
    attachments_of(&mut after)[0].name = "renamed.png".into();
    rejected(&after, "was modified");

    let mut after = before.clone();
    attachments_of(&mut after)[0].creator = bob;
    rejected(&after, "was modified");

    // Records are matched by id across blocks, so one changed while moving is caught.
    let mut after = before.clone();
    let mut moved = attachments_of(&mut after).remove(0);
    moved.size += 1;
    after.push(DraftItem {
        id: ItemId::new(),
        creator: bob,
        body: String::new(),
        kind: DraftItemKind::Prompt {
            attachments: vec![moved],
        },
    });
    rejected(&after, "was modified");
}

// 10. Anchors

fn text_edit(range: std::ops::Range<usize>, insert: &str) -> TextEdit {
    TextEdit {
        range,
        insert: insert.to_owned(),
    }
}

/// Byte offset of the first occurrence of `needle` in `item`'s body.
fn offset_of(draft: &Draft, item: ItemId, needle: &str) -> usize {
    draft.body(item).unwrap().find(needle).unwrap()
}

/// Inserts `text` at the end of `item`'s body.
fn append(draft: &Draft, item: ItemId, text: &str) {
    let len = draft.body(item).unwrap().len();
    assert!(draft.edit_body(item, &text_edit(len..len, text)));
}

/// Every offset of `item`'s body round-trips on both replicas when on a char boundary and is
/// refused otherwise.
fn assert_round_trips(a: &Draft, b: &Draft, item: ItemId) {
    let body = a.body(item).unwrap();
    assert_eq!(b.body(item).as_deref(), Some(body.as_str()));
    for offset in 0..=body.len() + 1 {
        let anchor = a.anchor(item, offset);
        if offset <= body.len() && body.is_char_boundary(offset) {
            let anchor = anchor.unwrap_or_else(|| panic!("no anchor at {offset} of {body:?}"));
            assert_eq!(a.resolve_anchor(item, &anchor), Some(offset), "{body:?}");
            assert_eq!(b.resolve_anchor(item, &anchor), Some(offset), "{body:?}");
        } else {
            assert_eq!(anchor, None, "offset {offset} of {body:?}");
        }
    }
}

#[test]
fn anchor_round_trip_with_multibyte_text() {
    let draft = Draft::new();
    let crab = draft.create_prompt(participant(), "é🦀");
    let empty = draft.create_prompt(participant(), "");
    let replica = replica_of(&draft);

    // Start, between the chars, end; inside "é" (1) and inside "🦀" (3..6) is refused.
    for (offset, expected) in [
        (0, true),
        (1, false),
        (2, true),
        (3, false),
        (5, false),
        (6, true),
    ] {
        assert_eq!(draft.anchor(crab, offset).is_some(), expected, "{offset}");
    }
    assert_round_trips(&draft, &replica, crab);

    assert!(draft.anchor(empty, 0).is_some());
    assert_eq!(draft.anchor(empty, 1), None);
    assert_round_trips(&draft, &replica, empty);

    assert_eq!(draft.anchor(ItemId::new(), 0), None);
}

#[test]
fn anchor_round_trip_across_many_items() {
    // Multi-byte text before the anchor within the same Yrs item is what the byte-offset
    // conversion has to get right, so build a body out of several items, some split remotely.
    let a = Draft::new();
    let id = a.create_prompt(participant(), "éé🦀ab");
    let b = replica_of(&a);
    assert_round_trips(&a, &b, id);

    assert!(a.edit_body(id, &text_edit(0..0, "ü🦀")));
    append(&a, id, "ñ😀z");
    assert!(b.edit_body(id, &text_edit(4..4, "日本")));
    sync(&a, &b);
    assert!(b.edit_body(id, &text_edit(0..2, "")));
    let before_a = offset_of(&a, id, "a");
    assert!(a.edit_body(id, &text_edit(before_a..before_a, "ß")));
    sync(&a, &b);

    assert_round_trips(&a, &b, id);
    assert_round_trips(&b, &a, id);
}

#[test]
fn anchor_follows_remote_edits() {
    let a = Draft::new();
    let id = a.create_prompt(participant(), "héllo 🦀 wörld");
    let b = replica_of(&a);

    let before_w = offset_of(&a, id, "w");
    let anchor = a.anchor(id, before_w).unwrap();

    // B inserts before the anchor and deletes after it; A edits before it concurrently.
    assert!(b.edit_body(id, &text_edit(0..0, "¡Hola! ")));
    let rld = offset_of(&b, id, "rld");
    assert!(b.edit_body(id, &text_edit(rld..rld + 3, "")));
    assert!(a.edit_body(id, &text_edit(1..3, "e")));
    // Not synced yet: each side resolves against what it has.
    assert_eq!(a.resolve_anchor(id, &anchor), Some(before_w - 1));
    assert_eq!(
        b.resolve_anchor(id, &anchor),
        Some(before_w + "¡Hola! ".len())
    );

    sync(&a, &b);
    assert_eq!(a.body(id).as_deref(), Some("¡Hola! hello 🦀 wö"));
    let expected = "¡Hola! hello 🦀 ".len();
    assert_eq!(a.resolve_anchor(id, &anchor), Some(expected));
    assert_eq!(b.resolve_anchor(id, &anchor), Some(expected));
}

#[test]
fn insert_at_anchor_lands_before_it() {
    let a = Draft::new();
    let id = a.create_prompt(participant(), "ab🦀cd");
    let b = replica_of(&a);
    let anchor = a.anchor(id, 2).unwrap();

    // Someone else types exactly at the anchor: the anchor stays with "🦀", after their text.
    assert!(b.edit_body(id, &text_edit(2..2, "XY")));
    sync(&a, &b);
    assert_eq!(a.body(id).as_deref(), Some("abXY🦀cd"));
    assert_eq!(a.resolve_anchor(id, &anchor), Some(4));
    assert_eq!(b.resolve_anchor(id, &anchor), Some(4));

    // The same happens when the anchor's owner types there, so they re-anchor after typing.
    assert!(a.edit_body(id, &text_edit(4..4, "é")));
    assert_eq!(a.resolve_anchor(id, &anchor), Some(6));
    sync(&a, &b);

    // Text inserted right after the anchored char doesn't move the anchor.
    assert!(b.edit_body(id, &text_edit(10..10, "!")));
    sync(&a, &b);
    assert_eq!(a.body(id).as_deref(), Some("abXYé🦀!cd"));
    assert_eq!(b.resolve_anchor(id, &anchor), Some(6));
}

#[test]
fn anchor_at_end_stays_at_end() {
    let a = Draft::new();
    let id = a.create_prompt(participant(), "hi 🦀");
    let empty = a.create_prompt(participant(), "");
    let b = replica_of(&a);
    let end = a.anchor(id, "hi 🦀".len()).unwrap();
    let empty_end = a.anchor(empty, 0).unwrap();

    // The anchor's owner keeps typing at the end: their caret stays at the end for everyone.
    append(&a, id, "é");
    append(&a, id, "!");
    assert_eq!(a.resolve_anchor(id, &end), Some(10));
    sync(&a, &b);
    assert_eq!(b.resolve_anchor(id, &end), Some(10));

    // Someone else appending moves it along too, like inserting at any other anchor.
    append(&b, id, " ok");
    sync(&a, &b);
    assert_eq!(a.body(id).as_deref(), Some("hi 🦀é! ok"));
    assert_eq!(a.resolve_anchor(id, &end), Some(13));
    assert_eq!(b.resolve_anchor(id, &end), Some(13));

    assert_eq!(b.resolve_anchor(empty, &empty_end), Some(0));
    assert!(b.set_body(empty, "日本").is_some());
    sync(&a, &b);
    assert_eq!(a.resolve_anchor(empty, &empty_end), Some(6));

    // Deleting everything brings it back to 0.
    assert!(a.set_body(id, "").is_some());
    assert_eq!(a.resolve_anchor(id, &end), Some(0));
}

#[test]
fn anchor_into_deleted_text() {
    let a = Draft::new();
    let id = a.create_prompt(participant(), "héllo wörld 🦀");
    let b = replica_of(&a);
    let at_o = a.anchor(id, offset_of(&a, id, "ö")).unwrap();
    let at_crab = a.anchor(id, offset_of(&a, id, "🦀")).unwrap();

    // Delete "o wörl" on B: the anchor on "ö" falls back to where the deleted text was.
    let start = offset_of(&b, id, "o w");
    assert!(b.edit_body(id, &text_edit(start..offset_of(&b, id, "d"), "")));
    sync(&a, &b);
    assert_eq!(a.body(id).as_deref(), Some("hélld 🦀"));
    for draft in [&a, &b] {
        assert_eq!(draft.resolve_anchor(id, &at_o), Some(start));
        assert_eq!(
            draft.resolve_anchor(id, &at_crab),
            Some(offset_of(&a, id, "🦀"))
        );
    }

    // Replace the whole body: every anchor resolves to a valid offset.
    assert!(a.set_body(id, "日本語").is_some());
    sync(&a, &b);
    for draft in [&a, &b] {
        for anchor in [&at_o, &at_crab] {
            let offset = draft.resolve_anchor(id, anchor).unwrap();
            let body = draft.body(id).unwrap();
            assert!(offset <= body.len() && body.is_char_boundary(offset));
        }
    }

    assert!(a.set_body(id, "").is_some());
    assert_eq!(a.resolve_anchor(id, &at_o), Some(0));
}

#[test]
fn anchor_before_its_text_arrives() {
    let a = Draft::new();
    let id = a.create_prompt(participant(), "abc");
    let b = replica_of(&a);
    append(&a, id, "déf");
    let anchor = a.anchor(id, offset_of(&a, id, "f")).unwrap();

    assert_eq!(b.resolve_anchor(id, &anchor), None);
    sync(&a, &b);
    assert_eq!(b.resolve_anchor(id, &anchor), Some(6));
}

#[test]
fn resolve_anchor_rejects_other_items_and_removed_items() {
    let a = Draft::new();
    let first = a.create_prompt(participant(), "first");
    let second = a.create_comment(participant(), target("quote"), "second");
    let empty = a.create_prompt(participant(), "");
    let b = replica_of(&a);

    let anchors = [
        a.anchor(first, 0).unwrap(),
        a.anchor(first, 2).unwrap(),
        a.anchor(first, 5).unwrap(),
        a.anchor(empty, 0).unwrap(),
    ];
    for anchor in &anchors {
        assert_eq!(a.resolve_anchor(second, anchor), None);
        assert_eq!(b.resolve_anchor(second, anchor), None);
        assert_eq!(a.resolve_anchor(ItemId::new(), anchor), None);
    }
    let second_anchor = a.anchor(second, 3).unwrap();
    assert_eq!(a.resolve_anchor(first, &second_anchor), None);
    assert_eq!(a.resolve_anchor(second, &second_anchor), Some(3));

    a.remove_items(&[first, empty]);
    sync(&a, &b);
    for anchor in &anchors {
        for draft in [&a, &b] {
            assert_eq!(draft.resolve_anchor(first, anchor), None);
            assert_eq!(draft.resolve_anchor(empty, anchor), None);
        }
    }
    assert_eq!(b.resolve_anchor(second, &second_anchor), Some(3));
}

#[test]
fn resolve_anchor_rejects_garbage() {
    let draft = Draft::new();
    let id = draft.create_prompt(participant(), "héllo");
    let valid = draft.anchor(id, 1).unwrap();

    let root = StickyIndex::new(IndexScope::Root(ITEMS.into()), Assoc::After).encode_v1();
    let unknown_client =
        StickyIndex::from_id(ID::new(ClientID::new(12345), 0), Assoc::After).encode_v1();
    let mut trailing = valid.clone();
    trailing.push(0);
    // A client id beyond 53 bits, which Yrs' own decoder debug-asserts on.
    let huge_client = [
        0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 0, 0,
    ];
    let cases: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![3, 0],
        vec![0xff; 16],
        valid[..valid.len() - 1].to_vec(),
        trailing,
        huge_client.to_vec(),
        vec![1, 5, b'i', b't'],
        root,
        unknown_client,
    ];
    for bytes in &cases {
        assert_eq!(draft.resolve_anchor(id, bytes), None, "{bytes:?}");
    }

    // Arbitrary bytes never panic, and whatever they resolve to is a valid offset. Half the
    // inputs use only small bytes, which often form well-formed anchors with bogus ids.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for len in 0..4096 {
        let modulus = if len % 2 == 0 { 8 } else { 256 };
        let bytes: Vec<u8> = (0..len % 24)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % modulus) as u8
            })
            .collect();
        if let Some(offset) = draft.resolve_anchor(id, &bytes) {
            assert!(draft.body(id).unwrap().is_char_boundary(offset));
        }
    }

    assert_eq!(draft.resolve_anchor(id, &valid), Some(1));
}

#[test]
fn anchors_are_standard_sticky_indices_and_record_nothing() {
    let draft = Draft::new();
    let id = draft.create_prompt(participant(), "é🦀x");
    draft.take_local_update();

    for offset in [0, 2, 6, 7] {
        let anchor = draft.anchor(id, offset).unwrap();
        let decoded = StickyIndex::decode_v1(&anchor).unwrap();
        assert_eq!(anchor::decode_anchor(&anchor).as_ref(), Some(&decoded));
        assert_eq!(decoded.assoc, Assoc::After);
        assert_eq!(draft.resolve_anchor(id, &anchor), Some(offset));
    }

    // `Before` anchors (not made by this crate) resolve right after their char.
    let at_crab = StickyIndex::decode_v1(&draft.anchor(id, 2).unwrap()).unwrap();
    let before = StickyIndex::from_id(*at_crab.id().unwrap(), Assoc::Before);
    assert_eq!(draft.resolve_anchor(id, &before.encode_v1()), Some(6));

    assert_eq!(draft.take_local_update(), None);
}
