//! The agent instruction is embedded from the same maintained file the local skill uses.
pub const NAME: &str = "telegram-rich-posts";
pub const URI: &str = "skill://telegram-rich-posts/SKILL.md";
pub const TEXT: &str = include_str!("../../.claude/skills/telegram-rich-posts/SKILL.md");
pub const TITLE: &str = "Rich-посты в Telegram";

pub fn description() -> &'static str {
    TEXT.strip_prefix("---\n")
        .or_else(|| TEXT.strip_prefix("---\r\n"))
        .and_then(|head| {
            head.lines()
                .take_while(|line| *line != "---")
                .find_map(|line| line.strip_prefix("description:").map(str::trim))
        })
        .unwrap_or("Подготовка и безопасная публикация rich-постов Telegram через Rich Studio")
}
