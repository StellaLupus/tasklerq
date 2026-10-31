//! Модель экрана и рендерер RichMessage.
//!
//! Экран описывается как список блоков с действиями на кнопках. Рендерер
//! превращает его в HTML и подставляет по одному callback-токену на кнопку.
//! Он не знает ни про базу, ни про домен: токены ему передаются готовыми, а
//! пользовательский текст всегда экранируется.

use super::api::RichMessage;

/// Максимум кнопок в одном ряду: длинные подписи иначе обрезаются.
pub const MAX_BUTTONS_PER_ROW: usize = 3;

/// Фрагмент строки с форматированием.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// Обычный текст.
    Text(String),
    /// Жирный текст.
    Bold(String),
    /// Курсив.
    Italic(String),
    /// Моноширинный текст (ссылки, коды).
    Code(String),
}

impl Span {
    /// Обычный текст.
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// Жирный текст.
    pub fn bold(value: impl Into<String>) -> Self {
        Self::Bold(value.into())
    }
}

/// Кнопка с действием.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button<A> {
    /// Подпись.
    pub label: String,
    /// Действие, выполняемое при нажатии.
    pub action: A,
}

/// Блок экрана.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block<A> {
    /// Заголовок.
    Heading(String),
    /// Абзац из фрагментов; `\n` внутри текста превращается в перенос строки.
    Paragraph(Vec<Span>),
    /// Горизонтальная черта.
    Rule,
    /// Сворачиваемый блок для длинных описаний.
    Details {
        /// Заголовок.
        summary: String,
        /// Текст.
        body: String,
    },
    /// Ряд кнопок.
    Buttons(Vec<Button<A>>),
}

/// Экран: упорядоченные блоки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View<A> {
    /// Блоки сверху вниз.
    pub blocks: Vec<Block<A>>,
}

impl<A> Default for View<A> {
    fn default() -> Self {
        Self { blocks: Vec::new() }
    }
}

impl<A> View<A> {
    /// Пустой экран.
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавить заголовок.
    pub fn heading(mut self, text: impl Into<String>) -> Self {
        self.blocks.push(Block::Heading(text.into()));
        self
    }

    /// Добавить абзац простого текста.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.blocks
            .push(Block::Paragraph(vec![Span::Text(text.into())]));
        self
    }

    /// Добавить абзац из фрагментов.
    pub fn spans(mut self, spans: Vec<Span>) -> Self {
        self.blocks.push(Block::Paragraph(spans));
        self
    }

    /// Добавить жирный абзац (заголовок секции).
    pub fn bold(mut self, text: impl Into<String>) -> Self {
        self.blocks
            .push(Block::Paragraph(vec![Span::Bold(text.into())]));
        self
    }

    /// Добавить черту.
    pub fn rule(mut self) -> Self {
        self.blocks.push(Block::Rule);
        self
    }

    /// Добавить сворачиваемый блок.
    pub fn details(mut self, summary: impl Into<String>, body: impl Into<String>) -> Self {
        self.blocks.push(Block::Details {
            summary: summary.into(),
            body: body.into(),
        });
        self
    }

    /// Добавить ряд кнопок; пустой ряд пропускается.
    pub fn row(mut self, buttons: Vec<Button<A>>) -> Self {
        if !buttons.is_empty() {
            self.blocks.push(Block::Buttons(buttons));
        }
        self
    }

    /// Кнопки в порядке отображения: по ним выпускаются токены.
    pub fn actions(&self) -> Vec<&A> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Buttons(buttons) => Some(buttons.iter().map(|button| &button.action)),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Число кнопок.
    pub fn button_count(&self) -> usize {
        self.actions().len()
    }
}

/// Кнопка с подписью и действием.
pub fn button<A>(label: impl Into<String>, action: A) -> Button<A> {
    Button {
        label: label.into(),
        action,
    }
}

/// Экранировать пользовательский текст для HTML и убрать управляющие символы.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' | '\t' => out.push(ch),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

fn escape_inline(text: &str) -> String {
    escape(text).replace('\t', " ").replace('\n', "<br/>")
}

fn render_span(span: &Span) -> String {
    match span {
        Span::Text(text) => escape_inline(text),
        Span::Bold(text) => format!("<b>{}</b>", escape_inline(text)),
        Span::Italic(text) => format!("<i>{}</i>", escape_inline(text)),
        Span::Code(text) => format!("<code>{}</code>", escape_inline(text)),
    }
}

/// Собрать HTML экрана. `tokens` — по одному на кнопку в порядке
/// [`View::actions`].
///
/// # Panics
/// Если число токенов не совпадает с числом кнопок: это ошибка вызывающего
/// кода, а не входных данных.
pub fn render<A>(view: &View<A>, tokens: &[String]) -> RichMessage {
    assert_eq!(
        tokens.len(),
        view.button_count(),
        "one callback token per button is required"
    );

    let mut tokens = tokens.iter();
    let mut lines: Vec<String> = Vec::new();

    for block in &view.blocks {
        match block {
            Block::Heading(text) => lines.push(format!("<h3>{}</h3>", escape_inline(text))),
            Block::Paragraph(spans) => {
                let body: String = spans.iter().map(render_span).collect();
                lines.push(format!("<p>{body}</p>"));
            }
            Block::Rule => lines.push("<hr/>".to_owned()),
            Block::Details { summary, body } => lines.push(format!(
                "<details><summary>{}</summary>{}</details>",
                escape_inline(summary),
                escape_inline(body)
            )),
            Block::Buttons(buttons) => {
                for chunk in buttons.chunks(MAX_BUTTONS_PER_ROW) {
                    let mut row = String::from("<tg-button-row>");
                    for button in chunk {
                        let token = tokens.next().expect("token count was checked");
                        row.push_str(&format!(
                            "<tg-button type=\"callback_data\" data=\"{}\">{}</tg-button>",
                            escape(token),
                            escape_inline(&button.label)
                        ));
                    }
                    row.push_str("</tg-button-row>");
                    lines.push(row);
                }
            }
        }
    }

    RichMessage {
        html: lines.join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_control_characters() {
        assert_eq!(
            escape("<b>&\"x\"\u{7}</b>"),
            "&lt;b&gt;&amp;&quot;x&quot;&lt;/b&gt;"
        );
    }

    #[test]
    fn renders_screen_with_buttons_in_rows_of_three() {
        let view = View::new()
            .heading("Задачи · 3")
            .spans(vec![Span::text("Просрочено: "), Span::bold("2")])
            .rule()
            .row(vec![
                button("A", 1),
                button("B", 2),
                button("C", 3),
                button("D", 4),
            ])
            .details("Описание", "строка 1\nстрока 2");
        let tokens: Vec<String> = (1..=4).map(|n| format!("t:{n:016}")).collect();

        let html = render(&view, &tokens).html;

        assert_eq!(
            html,
            "<h3>Задачи · 3</h3>\n\
             <p>Просрочено: <b>2</b></p>\n\
             <hr/>\n\
             <tg-button-row><tg-button type=\"callback_data\" data=\"t:0000000000000001\">A</tg-button>\
             <tg-button type=\"callback_data\" data=\"t:0000000000000002\">B</tg-button>\
             <tg-button type=\"callback_data\" data=\"t:0000000000000003\">C</tg-button></tg-button-row>\n\
             <tg-button-row><tg-button type=\"callback_data\" data=\"t:0000000000000004\">D</tg-button></tg-button-row>\n\
             <details><summary>Описание</summary>строка 1<br/>строка 2</details>"
        );
    }

    #[test]
    fn user_text_is_never_interpreted_as_markup() {
        let view = View::new()
            .heading("<script>alert(1)</script>")
            .text("a < b & c > d")
            .row(vec![button("<b>Готово</b>", ())]);
        let html = render(&view, &["t:AAAAAAAAAAAAAAAA".to_owned()]).html;

        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("a &lt; b &amp; c &gt; d"));
        assert!(html.contains(">&lt;b&gt;Готово&lt;/b&gt;</tg-button>"));
    }

    #[test]
    #[should_panic(expected = "one callback token per button")]
    fn token_count_must_match_buttons() {
        let view = View::new().row(vec![button("A", ())]);
        render(&view, &[]);
    }

    #[test]
    fn empty_rows_are_skipped() {
        let view: View<()> = View::new().row(Vec::new()).text("x");
        assert_eq!(view.blocks.len(), 1);
    }
}
