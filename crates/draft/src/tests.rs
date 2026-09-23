use yrs::{Array, ArrayPrelim, Map, MapPrelim, Text, TextPrelim, Transact};

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
