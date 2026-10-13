use super::*;

// Shortened from the templates these model families ship in their GGUFs.
const QWEN: &str = "{%- if messages[0]['role'] == 'system' %}{{- '<|im_start|>system\\n' + messages[0]['content'] + '<|im_end|>\\n' }}{%- endif %}";
const LLAMA3: &str = "{% set content = '<|start_header_id|>' + message['role'] + '<|end_header_id|>\\n\\n' + message['content'] | trim + '<|eot_id|>' %}";
const TINYLLAMA: &str = "{% for message in messages %}\n{% if message['role'] == 'user' %}\n{{ '<|user|>\\n' + message['content'] + eos_token }}\n{% elif message['role'] == 'assistant' %}\n{{ '<|assistant|>\\n'  + message['content'] + eos_token }}{% endif %}{% endfor %}";
const GEMMA: &str =
    "{{ '<start_of_turn>' + role + '\\n' + message['content'] | trim + '<end_of_turn>\\n' }}";
const GEMMA4: &str =
    "{{- '<|turn>' + role + '\\n' -}}{{- message['content'] -}}{{- '<turn|>\\n' -}}";
const MISTRAL: &str = "{{ bos_token }}{% for message in messages %}{{ '[INST] ' + message['content'] + ' [/INST]' }}{% endfor %}";
const GLM4: &str = "[gMASK]<sop>{% for item in messages %}{% if item['role'] == 'user' %}<|user|>\n{{ item['content'] }}{% elif item['role'] == 'assistant' %}<|assistant|>{% endif %}{% endfor %}";

#[test]
fn detects_known_families_and_refuses_to_guess() {
    let all = |_: &str| true;
    let cases = [
        (Some(QWEN), Some(ChatFormat::ChatMl)),
        (Some(LLAMA3), Some(ChatFormat::Llama3)),
        (Some(TINYLLAMA), Some(ChatFormat::Zephyr)),
        (Some(GEMMA), Some(ChatFormat::Gemma)),
        (Some(GEMMA4), Some(ChatFormat::Gemma4)),
        (Some(MISTRAL), Some(ChatFormat::Inst)),
        (Some(GLM4), None),
        (Some("{{ messages }}"), None),
        (None, None),
    ];
    for (template, expected) in cases {
        assert_eq!(ChatFormat::detect(template, all), expected, "{template:?}");
    }
    // Token-based formats need their markers in the vocabulary.
    assert_eq!(ChatFormat::detect(Some(QWEN), |_| false), None);
    assert_eq!(
        ChatFormat::detect(Some(LLAMA3), |m| m != "<|start_header_id|>"),
        None
    );
    // TinyLlama's markers are plain text in a SentencePiece vocabulary.
    assert_eq!(
        ChatFormat::detect(Some(TINYLLAMA), |_| false),
        Some(ChatFormat::Zephyr)
    );
}

#[test]
fn user_turns_end_where_the_reply_starts() {
    assert_eq!(
        ChatFormat::ChatMl.user_turn("hello"),
        "<|im_start|>user\nhello<|im_end|>\n<|im_start|>assistant\n"
    );
    assert_eq!(
        ChatFormat::Zephyr.user_turn("hi"),
        "<|user|>\nhi</s>\n<|assistant|>\n"
    );
    assert_eq!(
        ChatFormat::Gemma4.user_turn("hi"),
        "<|turn>user\nhi<turn|>\n<|turn>model\n"
    );
    assert_eq!(ChatFormat::Inst.user_turn("hi"), "[INST] hi [/INST]");
    assert!(
        ChatFormat::Llama3
            .user_turn("hi")
            .ends_with("<|start_header_id|>assistant<|end_header_id|>\n\n")
    );
    assert!(
        ChatFormat::Gemma
            .user_turn("hi")
            .ends_with("<start_of_turn>model\n")
    );
    assert_eq!(
        ChatFormat::Zephyr.conversation(Some("Be kind."), "hi"),
        "<|system|>\nBe kind.</s>\n<|user|>\nhi</s>\n<|assistant|>\n",
    );
    assert_eq!(
        ChatFormat::ChatMl.conversation(Some("Be kind."), "hi"),
        "<|im_start|>system\nBe kind.<|im_end|>\n<|im_start|>user\nhi<|im_end|>\n<|im_start|>assistant\n",
    );
    assert!(
        ChatFormat::Llama3
            .conversation(Some("Be kind."), "hi")
            .starts_with("<|start_header_id|>system<|end_header_id|>\n\nBe kind.<|eot_id|>")
    );
    // No system role: it leads the user's turn.
    assert_eq!(
        ChatFormat::Inst.conversation(Some("Be kind."), "hi"),
        "[INST] Be kind.\n\nhi [/INST]"
    );
    assert_eq!(
        ChatFormat::Gemma.conversation(Some("Be kind."), "hi"),
        "<start_of_turn>user\nBe kind.\n\nhi<end_of_turn>\n<start_of_turn>model\n"
    );
    for format in [
        ChatFormat::ChatMl,
        ChatFormat::Llama3,
        ChatFormat::Zephyr,
        ChatFormat::Gemma,
        ChatFormat::Gemma4,
        ChatFormat::Inst,
    ] {
        assert_eq!(
            format.conversation(None, "hi"),
            format.user_turn("hi"),
            "{format:?}"
        );
    }
    let names: Vec<_> = [
        ChatFormat::ChatMl,
        ChatFormat::Llama3,
        ChatFormat::Zephyr,
        ChatFormat::Gemma,
        ChatFormat::Gemma4,
        ChatFormat::Inst,
    ]
    .map(ChatFormat::name)
    .into();
    assert_eq!(
        names,
        ["chatml", "llama3", "zephyr", "gemma", "gemma4", "inst"]
    );
}

#[test]
fn every_format_round_trips_its_name_and_builds_turns_from_its_markers() {
    for format in ChatFormat::ALL {
        assert_eq!(ChatFormat::from_name(format.name()), Some(format));
        let markers = format.markers();
        assert_eq!(
            format.user_turn("hi"),
            format!(
                "{}hi{}{}",
                markers.user.0, markers.user.1, markers.assistant.0
            ),
            "{format:?}"
        );
    }
    assert_eq!(ChatFormat::from_name("jinja"), None);
    assert_eq!(ChatFormat::ChatMl.markers().assistant.1, "<|im_end|>\n");
    assert_eq!(ChatFormat::Inst.markers().assistant, ("", "</s>"));
}
