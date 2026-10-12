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
        if let Some(&(marker, token, format)) = MARKERS.iter().find(|(marker, ..)| template.contains(marker)) {
            return (!token || known(marker)).then_some(format);
        }
        let zephyr = template.contains("<|user|>") && template.contains("<|assistant|>") && !template.contains("[gMASK]");
        zephyr.then_some(Self::Zephyr)
    }

    /// (system turn open/close if the format has a system role, user turn
    /// open/close, start of the assistant's reply).
    const fn turns(self) -> (Option<(&'static str, &'static str)>, (&'static str, &'static str), &'static str) {
        match self {
            Self::ChatMl => (
                Some(("<|im_start|>system\n", "<|im_end|>\n")),
                ("<|im_start|>user\n", "<|im_end|>\n"),
                "<|im_start|>assistant\n",
            ),
            Self::Llama3 => (
                Some(("<|start_header_id|>system<|end_header_id|>\n\n", "<|eot_id|>")),
                ("<|start_header_id|>user<|end_header_id|>\n\n", "<|eot_id|>"),
                "<|start_header_id|>assistant<|end_header_id|>\n\n",
            ),
            Self::Zephyr => (Some(("<|system|>\n", "</s>\n")), ("<|user|>\n", "</s>\n"), "<|assistant|>\n"),
            Self::Gemma => (None, ("<start_of_turn>user\n", "<end_of_turn>\n"), "<start_of_turn>model\n"),
            Self::Gemma4 => (None, ("<|turn>user\n", "<turn|>\n"), "<|turn>model\n"),
            Self::Inst => (None, ("[INST] ", " [/INST]"), ""),
        }
    }

    /// `text` as one user turn, ending where the assistant's reply begins.
    pub fn user_turn(self, text: &str) -> String {
        self.conversation(None, text)
    }

    /// An optional system message, then one user turn.
    pub fn conversation(self, system: Option<&str>, text: &str) -> String {
        let (system_turn, (open, close), reply) = self.turns();
        match (system, system_turn) {
            (Some(system), Some((s_open, s_close))) => format!("{s_open}{system}{s_close}{open}{text}{close}{reply}"),
            (Some(system), None) => format!("{open}{system}\n\n{text}{close}{reply}"),
            (None, _) => format!("{open}{text}{close}{reply}"),
        }
    }
}

#[cfg(test)]
mod tests;
