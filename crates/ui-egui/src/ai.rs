//! AI tools through publik API or the person's own key, desktop only. The setup flow, the
//! credential file and the HTTP calls live in the `craft-publik` crate (shared by the Craft
//! apps). This module is DeckCraft's thin host: the first-launch dialog, the AI section of
//! File › Options, and the Insert tab's "Slide Text" and "Generate Image".
//!
//! Nothing is sent, and nothing is charged, until the person clicks a button in one of the two
//! tool dialogs. Programmatic commands never reach these dialogs. The results land through the
//! app's own commands: `text.set` and `slide.new` for slide text, `insert.picture` for a picture.

use std::sync::mpsc::{Receiver, TryRecvError};

pub use craft_publik::AppInfo;
use craft_publik::client::{self, Fail, Reply};
use craft_publik::ui::{Action, AiState, spawn};
use craft_publik::{Line, Provider};
use deckcraft_engine::Target;
use deckcraft_model::{PhType, ShapeKind};
use egui::{RichText, Ui};
use serde_json::{Value, json};

use crate::SlideApp;
use crate::dialogs::Dialog;
use crate::theme;

/// The dialog ids this module draws.
pub const SETUP: &str = "aiSetup";
pub const SLIDE_TEXT: &str = "aiSlideText";
pub const IMAGE: &str = "aiImage";

const TEXT_SYSTEM: &str = "You write the text of one presentation slide for DeckCraft. From the person's prompt or outline, write a short title (at most 8 words) and 3 to 6 short bullets (at most 12 words each), in the language of the prompt. Reply with JSON only, no code fence and no other text: {\"title\": \"...\", \"bullets\": [\"...\", \"...\"]}. Plain text in every field: no markdown, no bullet characters.";

/// The most bullets a reply may put on a slide.
const MAX_BULLETS: usize = 8;

/// The picture shapes the image line offers, as (`size` parameter, label). Wide fits a 16:9 slide.
const IMAGE_SIZES: [(&str, &str); 3] = [("1536x1024", "Wide"), ("1024x1024", "Square"), ("1024x1536", "Tall")];

/// Index of Square in [`IMAGE_SIZES`]: the one shape publik API draws (1024x1024).
const SQUARE: usize = 1;

/// Whether a provider can draw shape `i`: publik API draws Square only; a person's own key has all three.
fn offers(publik: bool, i: usize) -> bool {
    !publik || i == SQUARE
}

/// The name a generated picture gets (also its alt text until the person changes it).
const IMAGE_NAME: &str = "Generated image.png";

pub struct Ai {
    pub state: AiState,
    frames: u32,
    first_run_checked: bool,
    /// The first-launch check asked for the setup flow; `frame` opens it when nothing else shows.
    open_setup: bool,
    text: Job,
    /// The last slide text, editable before it goes on a slide.
    draft: Option<SlideDraft>,
    image: Job,
    /// Index into [`IMAGE_SIZES`].
    image_size: usize,
    /// A generated picture that has not reached a slide yet (no presentation was open).
    image_bytes: Option<Vec<u8>>,
    /// What happened to the last picture ("The picture is on slide 3.").
    image_note: Option<String>,
}

/// One tool's request in flight and its last error.
#[derive(Default)]
struct Job {
    prompt: String,
    error: Option<String>,
    rx: Option<Receiver<Result<Reply, Fail>>>,
}

impl Job {
    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// The reply, once, when it arrives.
    fn poll(&mut self, state: &mut AiState) -> Option<Reply> {
        let rx = self.rx.take()?;
        match rx.try_recv() {
            Ok(Ok(r)) => {
                state.note_reply(&r);
                self.error = None;
                Some(r)
            }
            Ok(Err(f)) => {
                state.note_fail(&f);
                // A 402 or a retired key is the notice bar's job (one message, one link); the rest is ours.
                if state.notice.is_none() {
                    self.error = Some(f.message);
                }
                None
            }
            Err(TryRecvError::Empty) => {
                self.rx = Some(rx);
                None
            }
            Err(TryRecvError::Disconnected) => {
                self.error = Some("The request stopped unexpectedly. Try again.".into());
                None
            }
        }
    }
}

/// A slide's title and bullets, as the person can still edit them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SlideDraft {
    title: String,
    /// One bullet per line.
    bullets: String,
}

impl Ai {
    pub fn new(app: AppInfo) -> Self {
        Ai {
            state: AiState::new(app),
            frames: 0,
            first_run_checked: false,
            open_setup: false,
            text: Job::default(),
            draft: None,
            image: Job::default(),
            image_size: 0,
            image_bytes: None,
            image_note: None,
        }
    }
}

/// `PUBLIK_SELF_TEST=1`: provision for real and print one line (publikhq.com/skills/setup.md).
pub fn self_test(app: &AppInfo) -> String {
    craft_publik::self_test(app)
}

/// Once per frame, before the dialogs draw: finish background work, put a generated picture on
/// the slide, and open the setup dialog when it is due.
pub fn frame(app: &mut SlideApp) {
    let Some(ai) = app.ai.as_mut() else { return };
    ai.state.poll();
    if let Some(r) = ai.text.poll(&mut ai.state) {
        let (title, bullets) = parse_slide_text(&r.text);
        if title.is_empty() && bullets.is_empty() {
            ai.text.error = Some("The answer held no slide text. Try again, or change the prompt.".into());
        } else {
            ai.draft = Some(SlideDraft { title, bullets: bullets.join("\n") });
        }
    }
    let picture_arrived = match ai.image.poll(&mut ai.state) {
        Some(r) => {
            ai.image_bytes = Some(r.bytes);
            ai.image_note = None;
            true
        }
        None => false,
    };
    ai.frames = ai.frames.saturating_add(1);
    // The first launch asks once the window is up (a few frames in). "Later" ends that for good.
    if !ai.first_run_checked && ai.frames > 3 {
        ai.first_run_checked = true;
        if ai.state.wants_first_run() || ai.state.owes_first_card() {
            ai.open_setup = true;
        }
    }
    let open_setup = ai.open_setup;
    // A picture goes on the current slide when it arrives. With no slide to take it, it waits for
    // "Insert on this slide" in the dialog.
    if picture_arrived && app.show.is_none() && slide_ready(app).is_ok() {
        place_image(app);
    }
    if !open_setup || app.dialog.is_some() || app.show.is_some() || app.opening.is_some() {
        return;
    }
    if let Some(ai) = app.ai.as_mut() {
        ai.open_setup = false;
        ai.state.begin();
    }
    app.dialog = Some(Dialog::new(SETUP));
}

fn unavailable(ui: &mut Ui) {
    ui.label("AI tools are off in this session (DECKCRAFT_NO_PREFS).");
}

fn apply(app: &mut SlideApp, action: Action) -> bool {
    match action {
        Action::None => false,
        Action::OpenUrl(url) => {
            app.open_url(&url);
            false
        }
        Action::Close => true,
    }
}

/// A tool dialog before AI is set up. "Set up AI…" puts the setup flow in its place; the flow
/// returns to the tool when it closes. True when the person closes the tool.
fn needs_setup(state: &mut AiState, ui: &mut Ui, d: &mut Dialog) -> bool {
    ui.label("This tool needs AI. Set it up first: publik API (no key to paste) or your own key.");
    ui.add_space(6.0);
    if primary(ui, "Set up AI…", true) {
        state.begin();
        let back = d.id.clone();
        *d = Dialog::new(SETUP);
        d.params = json!({"returnId": back, "returnParams": {}});
        return false;
    }
    close_row(ui)
}

fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::bold(13.0)));
}

/// The action a dialog is for, in the accent colour (as OK in the app's other dialogs).
fn primary(ui: &mut Ui, label: &str, enabled: bool) -> bool {
    let t = theme::Tokens::get(ui.ctx());
    let b = egui::Button::new(RichText::new(label).color(t.accent_text)).fill(t.accent).min_size(egui::vec2(72.0, 24.0));
    ui.add_enabled(enabled, b).clicked()
}

fn secondary(ui: &mut Ui, label: &str) -> bool {
    ui.add(egui::Button::new(label).min_size(egui::vec2(72.0, 24.0))).clicked()
}

/// The dialog's last row: Close at the right. True when it is clicked.
fn close_row(ui: &mut Ui) -> bool {
    ui.add_space(8.0);
    let row = egui::vec2(ui.available_width(), 24.0);
    ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(egui::Align::Center), |ui| secondary(ui, "Close")).inner
}

/// The sentence on what a publik call costs and why, from the provisioning reply (or the crate's
/// own); for an own key, who charges for it.
fn cost_sentence(state: &AiState, provider: &Provider) -> String {
    match provider {
        Provider::Publik { .. } => state
            .credential
            .as_ref()
            .map(|c| c.cost_sentence.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| craft_publik::WHY_IT_COSTS.to_string()),
        Provider::Own { .. } => "Your own key's service charges for each picture, at its own price.".into(),
        Provider::NotSetUp => String::new(),
    }
}

// ---- the slide the results go on -----------------------------------------------------------------

/// The current slide of the open presentation, when results can go on it.
fn slide_ready(app: &SlideApp) -> Result<usize, String> {
    let d = app.session.active().ok_or("Open a presentation first.")?;
    if d.selection.target != Target::Slides {
        return Err("Close the master view first. The result goes on a slide.".into());
    }
    d.current_slide().ok_or("Add a slide first.")?;
    Ok(d.selection.slide)
}

/// The current slide's title placeholder and its main text placeholder, as shape ids. The main
/// one is the placeholder `slide.new` fills: `idx="1"` first, then a full-size one.
fn slide_placeholders(app: &SlideApp) -> Result<(Option<u32>, Option<u32>), String> {
    slide_ready(app)?;
    let shapes = app.session.active().and_then(|d| d.current_slide()).map(|s| s.shapes.as_slice()).unwrap_or(&[]);
    let text_shape = |sh: &&deckcraft_model::Shape| matches!(sh.kind, ShapeKind::Shape);
    let title = shapes.iter().filter(text_shape).find(|sh| sh.ph_type().is_some_and(PhType::is_title)).map(|sh| sh.id.0);
    let body = shapes
        .iter()
        .enumerate()
        .filter(|(_, sh)| text_shape(sh))
        .filter_map(|(i, sh)| {
            let ph = sh.ph.as_ref()?;
            let text = matches!(ph.kind, PhType::Body | PhType::Obj | PhType::SubTitle);
            let small = matches!(ph.size.as_deref(), Some("half" | "quarter"));
            text.then_some(((ph.idx != 1, small, i), sh.id.0))
        })
        .min()
        .map(|(_, id)| id);
    Ok((title, body))
}

/// Fill the current slide's title and text placeholders. Returns a note when a part had no place.
fn apply_to_slide(app: &mut SlideApp, draft: &SlideDraft) -> Result<Option<String>, String> {
    let (title_id, body_id) = slide_placeholders(app)?;
    let title = draft.title.trim();
    let bullets = bullets_text(&draft.bullets);
    if title.is_empty() && bullets.is_empty() {
        return Err("Write a title or a bullet first.".into());
    }
    let put_title = title_id.filter(|_| !title.is_empty());
    let put_body = body_id.filter(|_| !bullets.is_empty());
    if put_title.is_none() && put_body.is_none() {
        return Err("This slide has no placeholder for this text. Click New slide instead.".into());
    }
    if let Some(id) = put_title {
        app.run("text.set", json!({"id": id, "text": title}))?;
    }
    if let Some(id) = put_body {
        app.run("text.set", json!({"id": id, "text": bullets}))?;
    }
    Ok(if !title.is_empty() && put_title.is_none() {
        Some("This slide has no title placeholder, so the title is not on it.".into())
    } else if !bullets.is_empty() && put_body.is_none() {
        Some("This slide has no text placeholder, so the bullets are not on it. Click New slide to add them on a new slide.".into())
    } else {
        None
    })
}

/// Add a Title and Content slide after the current one, with the title and bullets.
fn new_slide(app: &mut SlideApp, draft: &SlideDraft) -> Result<(), String> {
    let has_layout = app.session.active().is_some_and(|d| {
        let master = d.current_slide().and_then(|s| d.doc.masters.iter().position(|m| m.layout(s.layout).is_some())).unwrap_or(0);
        deckcraft_engine::cmd::slide::find_layout(&d.doc, &json!("titleAndContent"), master).is_some()
    });
    let mut p = json!({"title": draft.title.trim(), "body": bullets_text(&draft.bullets)});
    if has_layout && let Some(o) = p.as_object_mut() {
        o.insert("layout".into(), json!("titleAndContent"));
    }
    app.run("slide.new", p).map(|_| ())
}

/// Put the generated picture on the current slide, sized to fit (`insert.picture` fits it).
fn place_image(app: &mut SlideApp) {
    let Some(bytes) = app.ai.as_mut().and_then(|ai| ai.image_bytes.take()) else { return };
    let slide = slide_ready(app);
    let placed = slide.and_then(|slide| {
        app.run("insert.picture", json!({"name": IMAGE_NAME, "data": deckcraft_engine::cmd::base64_encode(&bytes)})).map(|_| slide)
    });
    if let Some(ai) = app.ai.as_mut() {
        match placed {
            Ok(slide) => {
                ai.image_note = Some(format!("The picture is on slide {}.", slide.saturating_add(1)));
                ai.image.error = None;
            }
            Err(e) => {
                ai.image.error = Some(e);
                // Keep the picture: it is paid for, and "Insert on this slide" tries again.
                ai.image_bytes = Some(bytes);
            }
        }
    }
}

// ---- the model's answer --------------------------------------------------------------------------

/// The bullets as `text.set` takes them: one paragraph per line, blank lines left out.
fn bullets_text(bullets: &str) -> String {
    bullets.lines().map(clean_line).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// One line without markdown: headings, bold marks, bullet characters and list numbers go.
fn clean_line(s: &str) -> String {
    let mut s = s.trim().trim_start_matches('#').trim_start();
    for p in ["- ", "* ", "• ", "– ", "— ", "· "] {
        if let Some(r) = s.strip_prefix(p) {
            s = r;
            break;
        }
    }
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if (1..3).contains(&digits)
        && let Some(r) = s.get(digits..).and_then(|r| r.strip_prefix(". ").or_else(|| r.strip_prefix(") ")))
    {
        s = r;
    }
    s.trim().trim_matches('*').trim().trim_matches('"').trim().to_string()
}

/// "Title: Our plan" → "Our plan".
fn strip_label(s: String, label: &str) -> String {
    match s.split_once(':') {
        Some((head, rest)) if head.trim().eq_ignore_ascii_case(label) => rest.trim().to_string(),
        _ => s,
    }
}

/// The title and bullets out of a chat answer: the JSON the prompt asks for (also inside a code
/// fence or after a sentence), else plain lines, the first being the title.
fn parse_slide_text(text: &str) -> (String, Vec<String>) {
    let t = text.trim();
    if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}'))
        && a < b
        && let Some(v) = t.get(a..=b).and_then(|s| serde_json::from_str::<Value>(s).ok())
    {
        let title = v.get("title").and_then(Value::as_str).map(clean_line).unwrap_or_default();
        let bullets: Vec<String> = v
            .get("bullets")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(clean_line).filter(|s| !s.is_empty()).take(MAX_BULLETS).collect())
            .unwrap_or_default();
        if !title.is_empty() || !bullets.is_empty() {
            return (title, bullets);
        }
    }
    let mut lines =
        t.lines().map(str::trim).filter(|l| !l.starts_with("```")).map(clean_line).filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("bullets:"));
    let title = lines.next().map(|l| strip_label(l, "title")).unwrap_or_default();
    (title, lines.take(MAX_BULLETS).collect())
}

// ---- dialogs ------------------------------------------------------------------------------------

/// The setup flow: two cards (publik API, your own key), the disclosure and the first-run card.
pub fn setup_dialog(app: &mut SlideApp, ui: &mut Ui) -> bool {
    let Some(ai) = app.ai.as_mut() else {
        unavailable(ui);
        return close_row(ui);
    };
    let action = ai.state.setup_ui(ui);
    apply(app, action)
}

/// Insert › Slide Text: a prompt or outline in, a title and bullets out, then onto a slide.
pub fn slide_text_dialog(app: &mut SlideApp, ui: &mut Ui, d: &mut Dialog) -> bool {
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else {
        unavailable(ui);
        return close_row(ui);
    };
    let provider = ai.state.provider();
    if !provider.is_set_up() {
        return needs_setup(&mut ai.state, ui, d);
    }
    let mut action = Action::None;
    let mut start = false;
    let mut to_slide = false;
    let mut add_slide = false;
    ui.label("Write a prompt or a short outline. The model writes a title and 3 to 6 bullets.");
    ui.add_space(4.0);
    ui.add_enabled(
        !ai.text.busy(),
        egui::TextEdit::multiline(&mut ai.text.prompt)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("Three reasons to hold the team offsite in Lisbon"),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ai.text.busy() {
            ui.spinner();
            ui.label("Writing…");
        } else if primary(ui, "Write slide text", !ai.text.prompt.trim().is_empty()) {
            start = true;
        }
    });
    if let Some(e) = &ai.text.error {
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    if let Some(draft) = ai.draft.as_mut() {
        ui.add_space(8.0);
        heading(ui, "Title");
        ui.add(egui::TextEdit::singleline(&mut draft.title).desired_width(f32::INFINITY));
        ui.add_space(4.0);
        heading(ui, "Bullets (one on each line)");
        ui.add(egui::TextEdit::multiline(&mut draft.bullets).desired_rows(4).desired_width(f32::INFINITY));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            to_slide = primary(ui, "Apply to this slide", true);
            add_slide = secondary(ui, "New slide");
        });
    }
    ui.add_space(4.0);
    let bar = ai.state.result_bar(ui);
    if bar != Action::None {
        action = bar;
    }
    let mut close = close_row(ui);
    if start {
        ai.draft = None;
        ai.text.error = None;
        let user = ai.text.prompt.trim().to_string();
        ai.text.rx = Some(spawn(&ctx, move || client::chat(&provider, TEXT_SYSTEM, &user)));
    }
    if (to_slide || add_slide)
        && let Some(draft) = ai.draft.clone()
    {
        let done = if to_slide { apply_to_slide(app, &draft) } else { new_slide(app, &draft).map(|()| None) };
        match done {
            Ok(None) => close = true,
            Ok(Some(note)) => {
                if let Some(ai) = app.ai.as_mut() {
                    ai.text.error = Some(note);
                }
            }
            Err(e) => {
                if let Some(ai) = app.ai.as_mut() {
                    ai.text.error = Some(e);
                }
            }
        }
    }
    apply(app, action) || close
}

/// Insert › Generate Image: a prompt in, a picture on the current slide.
pub fn image_dialog(app: &mut SlideApp, ui: &mut Ui, d: &mut Dialog) -> bool {
    let ctx = ui.ctx().clone();
    let can_place = slide_ready(app);
    let Some(ai) = app.ai.as_mut() else {
        unavailable(ui);
        return close_row(ui);
    };
    let provider = ai.state.provider();
    if !provider.is_set_up() {
        return needs_setup(&mut ai.state, ui, d);
    }
    let mut action = Action::None;
    let mut start = false;
    let mut place = false;
    let supported = provider.supports(Line::Images);
    ui.label("Describe the picture. It goes on the current slide, sized to fit, when it arrives.");
    ui.add_space(4.0);
    ui.add_enabled(
        !ai.image.busy(),
        egui::TextEdit::multiline(&mut ai.image.prompt)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("A watercolor of a lighthouse at dawn"),
    );
    ui.add_space(4.0);
    if !offers(provider.is_publik(), ai.image_size) {
        ai.image_size = SQUARE;
    }
    ui.horizontal(|ui| {
        ui.label("Shape:");
        for (i, (_, label)) in IMAGE_SIZES.iter().enumerate() {
            if !offers(provider.is_publik(), i) {
                continue;
            }
            ui.add_enabled_ui(!ai.image.busy(), |ui| ui.radio_value(&mut ai.image_size, i, *label));
        }
    });
    ui.add_space(4.0);
    if supported {
        // The price comes before the click that spends it.
        ui.label(RichText::new(cost_sentence(&ai.state, &provider)).weak());
    } else {
        ui.colored_label(ui.visuals().warn_fg_color, Fail::unsupported(Line::Images).message);
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ai.image.busy() {
            ui.spinner();
            ui.label("Generating… This can take a minute.");
        } else if primary(ui, "Generate image", supported && !ai.image.prompt.trim().is_empty()) {
            start = true;
        }
    });
    if let Some(e) = &ai.image.error {
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    if let Some(n) = &ai.image_note {
        ui.add_space(4.0);
        ui.label(n);
    }
    if ai.image_bytes.is_some() {
        ui.add_space(4.0);
        ui.label("The picture is ready but not on a slide yet.");
        place = primary(ui, "Insert on this slide", can_place.is_ok());
    }
    ui.add_space(4.0);
    let bar = ai.state.result_bar(ui);
    if bar != Action::None {
        action = bar;
    }
    let close = close_row(ui);
    if start {
        ai.image.error = None;
        ai.image_note = None;
        let prompt = ai.image.prompt.trim().to_string();
        let size = if provider.is_publik() { IMAGE_SIZES[SQUARE].0 } else { IMAGE_SIZES.get(ai.image_size).map_or("1024x1024", |s| s.0) };
        ai.image.rx = Some(spawn(&ctx, move || client::image_generate(&provider, &prompt, size)));
    }
    if place {
        place_image(app);
    }
    apply(app, action) || close
}

// ---- File › Options › AI -------------------------------------------------------------------------

/// The AI section of File › Options (Preferences). True when the person asked for the setup flow.
pub fn options_section(app: &mut SlideApp, ui: &mut Ui) -> bool {
    let Some(ai) = app.ai.as_mut() else {
        ui.label(RichText::new("AI tools are off in this session (DECKCRAFT_NO_PREFS).").weak());
        return false;
    };
    ui.label(RichText::new("Slide Text and Generate Image (Insert tab) run through publik API or your own key.").weak());
    ui.add_space(4.0);
    let mut open_setup = false;
    let action = ai.state.options_ui(ui, &mut open_setup);
    if open_setup {
        ai.state.begin();
    }
    apply(app, action);
    open_setup
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use deckcraft_engine::Session;

    #[test]
    fn json_answers_give_a_title_and_bullets() {
        let (t, b) = parse_slide_text("{\"title\": \"Why Lisbon\", \"bullets\": [\"Cheap flights\", \"- Good weather\", \"\"]}");
        assert_eq!(t, "Why Lisbon");
        assert_eq!(b, ["Cheap flights", "Good weather"]);
        let (t, b) = parse_slide_text("Here it is:\n```json\n{\"title\": \"**Plan**\", \"bullets\": [\"1. One\", \"2) Two\"]}\n```");
        assert_eq!(t, "Plan");
        assert_eq!(b, ["One", "Two"]);
    }

    #[test]
    fn plain_answers_give_a_title_and_bullets() {
        let (t, b) = parse_slide_text("Title: Our roadmap\n\n- Ship 1.0\n* Hire two people\n• Open an office\n");
        assert_eq!(t, "Our roadmap");
        assert_eq!(b, ["Ship 1.0", "Hire two people", "Open an office"]);
        let (t, b) = parse_slide_text("## Results\nBullets:\n1. Up 20%\n2. Down 3%");
        assert_eq!(t, "Results");
        assert_eq!(b, ["Up 20%", "Down 3%"]);
        assert_eq!(parse_slide_text("   "), (String::new(), vec![]));
    }

    #[test]
    fn bullets_are_capped_and_cleaned() {
        let many: Vec<String> = (0..20).map(|i| format!("\"b{i}\"")).collect();
        let (_, b) = parse_slide_text(&format!("{{\"title\": \"T\", \"bullets\": [{}]}}", many.join(",")));
        assert_eq!(b.len(), MAX_BULLETS);
        assert_eq!(bullets_text("- a\n\n  * b  \n"), "a\nb");
    }

    fn app_with(layout: &str) -> SlideApp {
        let mut app = SlideApp::new(Session::with_new(), Services::default());
        app.run("slide.new", json!({"layout": layout})).unwrap();
        app
    }

    fn texts(app: &SlideApp) -> Vec<String> {
        let d = app.session.active().unwrap();
        d.current_slide().unwrap().shapes.iter().map(|s| s.text.as_ref().map(|t| t.text()).unwrap_or_default()).collect()
    }

    #[test]
    fn slide_text_fills_the_title_and_content_placeholders() {
        let mut app = app_with("titleAndContent");
        let draft = SlideDraft { title: "Why Lisbon".into(), bullets: "Cheap flights\n- Good weather".into() };
        assert_eq!(apply_to_slide(&mut app, &draft), Ok(None));
        let t = texts(&app);
        assert!(t.iter().any(|x| x == "Why Lisbon"), "{t:?}");
        assert!(t.iter().any(|x| x == "Cheap flights\nGood weather"), "{t:?}");
    }

    #[test]
    fn slide_text_says_what_did_not_fit() {
        let mut app = app_with("titleOnly");
        let draft = SlideDraft { title: "Only a title".into(), bullets: "One\nTwo".into() };
        let note = apply_to_slide(&mut app, &draft).unwrap();
        assert!(note.is_some_and(|n| n.contains("New slide")));
        assert!(texts(&app).iter().any(|x| x == "Only a title"));
        let mut app = app_with("blank");
        assert!(apply_to_slide(&mut app, &draft).is_err());
    }

    #[test]
    fn new_slide_adds_a_title_and_content_slide() {
        let mut app = app_with("blank");
        let before = app.session.active().unwrap().doc.slides.len();
        let draft = SlideDraft { title: "Next".into(), bullets: "A\nB".into() };
        new_slide(&mut app, &draft).unwrap();
        let d = app.session.active().unwrap();
        assert_eq!(d.doc.slides.len(), before + 1);
        let t = texts(&app);
        assert!(t.iter().any(|x| x == "Next") && t.iter().any(|x| x == "A\nB"), "{t:?}");
    }

    fn harness(app: SlideApp, width: f32) -> egui_kittest::Harness<'static, SlideApp> {
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(width, 900.0)).build_ui_state(
            |ui, app: &mut SlideApp| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        );
        // The app keeps asking for frames, so step instead of run().
        h.run_steps(3);
        h
    }

    /// The Insert tab's AI group fits the default window (1440 points) beside the existing groups,
    /// and its menu opens both tools.
    #[test]
    fn the_insert_tab_shows_the_ai_group() {
        use egui_kittest::kittest::Queryable;
        let mut app = SlideApp::new(Session::with_new(), Services::default());
        app.ui.tab = "Insert".into();
        let mut h = harness(app, 1440.0);
        for label in ["Audio", "AI"] {
            let r = h.get_by_label(label).rect();
            assert!(r.right() <= 1440.0 - 24.0, "{label} is cut off: {r:?}");
        }
        for (item, id) in [("Slide Text…", SLIDE_TEXT), ("Generate Image…", IMAGE)] {
            h.get_by_label("AI").click();
            h.run_steps(3);
            h.get_by_label(item).click();
            h.run_steps(3);
            assert_eq!(h.state().dialog.as_ref().map(|d| d.id.as_str()), Some(id));
            // Without preferences (tests, agents' runs) the tools say so and send nothing.
            assert!(h.query_by_label("AI tools are off in this session (DECKCRAFT_NO_PREFS).").is_some());
            h.get_by_label("Close").click();
            h.run_steps(3);
            assert!(h.state().dialog.is_none());
        }
    }

    /// File › Options (Preferences) has an AI section.
    #[test]
    fn preferences_have_an_ai_section() {
        use egui_kittest::kittest::Queryable;
        let mut app = SlideApp::new(Session::with_new(), Services::default());
        app.run("app.preferences", json!({})).unwrap();
        let mut h = harness(app, 1400.0);
        h.run_steps(2);
        assert!(h.query_by_label("AI").is_some());
        assert!(h.query_by_label("AI tools are off in this session (DECKCRAFT_NO_PREFS).").is_some());
    }

    #[test]
    fn a_generated_picture_lands_on_the_current_slide() {
        let mut app = app_with("blank");
        // A 1x1 PNG.
        let png =
            deckcraft_engine::cmd::base64_decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
                .unwrap();
        let shapes = |app: &SlideApp| app.session.active().unwrap().current_slide().unwrap().shapes.len();
        let before = shapes(&app);
        let mut ai = Ai::new(AppInfo { slug: "deckcraft", name: "DeckCraft", version: "0", app_token: "", dialects: &[], config_dir: None });
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(Reply { bytes: png, ..Default::default() })).unwrap();
        ai.image.rx = Some(rx);
        app.ai = Some(ai);
        frame(&mut app);
        assert_eq!(shapes(&app), before + 1);
        let ai = app.ai.as_ref().unwrap();
        assert!(ai.image_bytes.is_none());
        assert_eq!(ai.image_note.as_deref(), Some("The picture is on slide 2."));
        // A picture that cannot go on a slide waits for the button; frames do not retry it.
        app.ai.as_mut().unwrap().image_bytes = Some(b"not a picture".to_vec());
        frame(&mut app);
        assert_eq!(shapes(&app), before + 1);
        place_image(&mut app);
        let ai = app.ai.as_ref().unwrap();
        assert!(ai.image_bytes.is_some() && ai.image.error.is_some(), "kept for another try");
        assert_eq!(shapes(&app), before + 1);
    }
}

#[cfg(test)]
mod publik_shape_tests {
    use super::{offers, IMAGE_SIZES, SQUARE};

    #[test]
    fn publik_api_offers_square_only() {
        assert_eq!(IMAGE_SIZES[SQUARE].0, "1024x1024");
        assert!(offers(true, SQUARE));
        assert!(!offers(true, 0));
        assert!(!offers(true, 2));
        assert!((0..IMAGE_SIZES.len()).all(|i| offers(false, i)));
    }
}
