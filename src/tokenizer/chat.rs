//! Chat prompts in the model's own format, detected from the GGUF chat template.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFormat {
    /// Qwen and other `<|im_start|>` models.
    ChatMl,
    Llama3,
    /// TinyLlama Chat and other `<|user|>` / `</s>` models.
    Zephyr,
    /// Gemma 2 and 3.
    Gemma,
    Gemma4,
    /// Mistral and Llama 2 `[INST]`.
    Inst,
}

impl ChatFormat {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ChatMl => "chatml",
            Self::Llama3 => "llama3",
            Self::Zephyr => "zephyr",
            Self::Gemma => "gemma",
            Self::Gemma4 => "gemma4",
            Self::Inst => "inst",
        }
    }

    /// `known` tells whether a marker is a single vocabulary token.
    pub(crate) fn detect(template: Option<&str>, known: impl Fn(&str) -> bool) -> Option<Self> {
        // (template marker, must be a vocabulary token, format); first match wins.
        const MARKERS: [(&str, bool, ChatFormat); 5] = [
            ("<|im_start|>", true, ChatFormat::ChatMl),
            ("<|start_header_id|>", true, ChatFormat::Llama3),
            ("<|turn>", false, ChatFormat::Gemma4),
            ("<start_of_turn>", true, ChatFormat::Gemma),
            ("[INST]", false, ChatFormat::Inst),
        ];
        let template = template?;
        if let Some(&(marker, token, format)) = MARKERS
            .iter()
            .find(|(marker, ..)| template.contains(marker))
        {
            return (!token || known(marker)).then_some(format);
        }
        let zephyr = template.contains("<|user|>")
            && template.contains("<|assistant|>")
            && !template.contains("[gMASK]");
        zephyr.then_some(Self::Zephyr)
    }

    pub const ALL: [Self; 6] = [
        Self::ChatMl,
        Self::Llama3,
        Self::Zephyr,
        Self::Gemma,
        Self::Gemma4,
        Self::Inst,
    ];

    /// The format reported as `name()`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.name() == name)
    }

    pub const fn markers(self) -> Markers {
        match self {
            Self::ChatMl => Markers {
                system: Some(("<|im_start|>system\n", "<|im_end|>\n")),
                user: ("<|im_start|>user\n", "<|im_end|>\n"),
                assistant: ("<|im_start|>assistant\n", "<|im_end|>\n"),
            },
            Self::Llama3 => Markers {
                system: Some((
                    "<|start_header_id|>system<|end_header_id|>\n\n",
                    "<|eot_id|>",
                )),
                user: ("<|start_header_id|>user<|end_header_id|>\n\n", "<|eot_id|>"),
                assistant: (
                    "<|start_header_id|>assistant<|end_header_id|>\n\n",
                    "<|eot_id|>",
                ),
            },
            Self::Zephyr => Markers {
                system: Some(("<|system|>\n", "</s>\n")),
                user: ("<|user|>\n", "</s>\n"),
                assistant: ("<|assistant|>\n", "</s>\n"),
            },
            Self::Gemma => Markers {
                system: None,
                user: ("<start_of_turn>user\n", "<end_of_turn>\n"),
                assistant: ("<start_of_turn>model\n", "<end_of_turn>\n"),
            },
            Self::Gemma4 => Markers {
                system: None,
                user: ("<|turn>user\n", "<turn|>\n"),
                assistant: ("<|turn>model\n", "<turn|>\n"),
            },
            Self::Inst => Markers {
                system: None,
                user: ("[INST] ", " [/INST]"),
                assistant: ("", "</s>"),
            },
        }
    }

    /// `text` as one user turn, ending where the assistant's reply begins.
    pub fn user_turn(self, text: &str) -> String {
        self.conversation(None, text)
    }

    /// An optional system message, then one user turn.
    pub fn conversation(self, system: Option<&str>, text: &str) -> String {
        let Markers {
            system: system_turn,
            user: (open, close),
            assistant: (reply, _),
        } = self.markers();
        match (system, system_turn) {
            (Some(system), Some((s_open, s_close))) => {
                format!("{s_open}{system}{s_close}{open}{text}{close}{reply}")
            }
            (Some(system), None) => format!("{open}{system}\n\n{text}{close}{reply}"),
            (None, _) => format!("{open}{text}{close}{reply}"),
        }
    }
}

/// A format's turns: each is `open`, the text, then `close`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Markers {
    /// `None` when the format has no system role.
    pub system: Option<(&'static str, &'static str)>,
    pub user: (&'static str, &'static str),
    /// Its `open` is where the model's reply starts.
    pub assistant: (&'static str, &'static str),
}

#[cfg(test)]
mod tests;
