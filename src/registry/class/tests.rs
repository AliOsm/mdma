use super::*;

#[test]
fn class_follows_task_tags() {
    let cases: &[(&[&str], Class)] = &[
        (&[], Class::Generative),
        (&["text-generation", "chat"], Class::Generative),
        (&["text-classification"], Class::Decision),
        (&["nli", "zero-shot-classification"], Class::Decision),
        (&["feature-extraction"], Class::Embedding),
        (&["sentence-similarity"], Class::Embedding),
        (&["text-ranking"], Class::Reranker),
    ];
    for (tags, expected) in cases {
        let tags: Vec<String> = tags.iter().map(|t| (*t).to_owned()).collect();
        assert_eq!(Class::from_tags(&tags), *expected, "tags: {tags:?}");
    }
}

#[test]
fn names_round_trip() {
    for class in Class::ALL {
        assert_eq!(Class::parse(class.as_str()), Some(class));
    }
    for capability in Capability::ALL {
        assert_eq!(Capability::parse(capability.as_str()), Some(capability));
    }
    assert_eq!(Class::parse("llm"), None);
    assert_eq!(Capability::parse("telepathy"), None);
}

#[test]
fn reasoning_comes_from_a_thinking_template() {
    assert_eq!(Capability::from_template("{{ '<|im_start|>assistant\\n<think>\\n' }}"), [Capability::Reasoning]);
    assert!(Capability::from_template("{{ '<|assistant|>' }}").is_empty());
}
