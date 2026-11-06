use accesskit::{HasPopup, Live, NodeId, Role, Toggled};

use super::*;
use crate::draw::{theme, Canvas, Rect};
use crate::ui::widgets::Paint;
use crate::ui::{BarHit, Tree};

const GROUP: u64 = 10;
const FIRST: u64 = 11;
const SECOND: u64 = 12;
const SELECT: u64 = 20;
const LIST: u64 = 21;
const ONE: u64 = 30;
const TWO: u64 = 31;

fn form(open: bool, choices: &[Choice], description: Option<&str>, width: f32) -> Tree {
    let mut font = Font::for_test();
    let mut f = Form::new(1, "Settings", "en-US", width);
    f.title(2, "Settings");
    let mut s = f.section(3, 4, "Look");
    f.segmented(
        &mut s,
        Segmented {
            id: GROUP,
            label: "Look",
            choices: &[Choice::new(FIRST, "Light"), Choice::new(SECOND, "Dark")],
            chosen: SECOND,
            description,
        },
    );
    f.select(
        &mut s,
        Select {
            id: SELECT,
            list: LIST,
            label: "Browser",
            choices,
            chosen: choices.last().map(|c| c.id),
            open,
            description: None,
        },
    );
    f.finish(&mut font)
}

fn browsers() -> [Choice<'static>; 2] {
    [Choice::new(ONE, "Chromium"), Choice::new(TWO, "Firefox")]
}

fn focus_of(tree: &Tree) -> NodeId {
    tree.accesskit(Rect::new(0.0, 0.0, 600.0, 2000.0), (0, 0)).1
}

#[test]
fn a_segmented_control_is_a_radio_group_the_keyboard_reaches() {
    let mut t = form(false, &browsers(), None, 600.0);
    let group = t.semantic(GROUP).unwrap();
    assert_eq!(group.role(), Role::RadioGroup);
    assert_eq!(group.label(), Some("Look"));
    assert_eq!(t.semantic(FIRST).unwrap().toggled(), Some(Toggled::False));
    assert_eq!(t.semantic(SECOND).unwrap().toggled(), Some(Toggled::True));
    t.step(true, 2000.0);
    assert_eq!(t.selected_id(), Some(GROUP));
    assert_eq!(
        focus_of(&t),
        NodeId(SECOND),
        "focus is on the checked radio"
    );
    let row = t.focused.unwrap();
    assert_eq!(t.radio_step(row, true), Some(FIRST), "arrows wrap");
    assert_eq!(t.radio_step(row, false), Some(FIRST));
    assert_eq!(t.row_of(FIRST), Some(row));
}

#[test]
fn a_closed_select_shows_its_value_and_hides_its_options() {
    let mut t = form(false, &browsers(), None, 600.0);
    let select = t.semantic(SELECT).unwrap();
    assert_eq!(select.role(), Role::ComboBox);
    assert_eq!(select.value(), Some("Firefox"));
    assert_eq!(select.is_expanded(), Some(false));
    assert_eq!(select.has_popup(), Some(HasPopup::Listbox));
    assert!(t.semantic(LIST).is_none());
    t.step(true, 2000.0);
    t.step(true, 2000.0);
    assert_eq!(t.selected_id(), Some(SELECT));
    t.step(true, 2000.0);
    assert_eq!(t.selected_id(), Some(GROUP), "two controls, then it wraps");
}

#[test]
fn an_open_select_lists_its_options_as_rows_and_keeps_the_choice() {
    let mut t = form(true, &browsers(), None, 600.0);
    let select = t.semantic(SELECT).unwrap();
    assert_eq!(select.is_expanded(), Some(true));
    assert_eq!(select.controls(), &[NodeId(LIST)]);
    assert_eq!(t.semantic(LIST).unwrap().role(), Role::ListBox);
    let ids: Vec<u64> = (0..4)
        .map(|_| {
            t.step(true, 2000.0);
            t.selected_id().unwrap()
        })
        .collect();
    assert_eq!(ids, [GROUP, SELECT, ONE, TWO]);
    assert_eq!(t.semantic(TWO).unwrap().is_selected(), Some(true));
    assert_eq!(t.semantic(ONE).unwrap().is_selected(), Some(false));
    t.select(t.row_of(ONE), 2000.0);
    assert_eq!(
        t.semantic(TWO).unwrap().is_selected(),
        Some(true),
        "the keyboard moving is not choosing"
    );
}

#[test]
fn a_select_with_no_options_is_disabled_and_skipped() {
    let mut t = form(false, &[], None, 600.0);
    assert!(t.semantic(SELECT).unwrap().is_disabled());
    t.step(true, 2000.0);
    t.step(true, 2000.0);
    assert_eq!(t.selected_id(), Some(GROUP));
}

#[test]
fn help_text_wraps_and_describes_its_control() {
    let text =
        "Open apps switch language the next time they start, so a browser keeps its own until then";
    let plain = form(false, &browsers(), None, 320.0);
    let helped = form(false, &browsers(), Some(text), 320.0);
    assert_eq!(helped.semantic(GROUP).unwrap().description(), Some(text));
    let grew = helped.size().1 - plain.size().1;
    assert!(grew > 2.0 * 12.0 * crate::ui::widgets::LEADING, "{grew}");
}

#[test]
fn names_in_another_language_carry_their_tag() {
    let mut font = Font::for_test();
    let mut f = Form::new(1, "设置", "zh-CN", 600.0);
    let mut s = f.section(3, 4, "语言");
    f.segmented(
        &mut s,
        Segmented {
            id: GROUP,
            label: "语言",
            choices: &[
                Choice::new(FIRST, "English (US)").in_lang("en-US"),
                Choice::new(SECOND, "简体中文").in_lang("zh-CN"),
            ],
            chosen: SECOND,
            description: None,
        },
    );
    let t = f.finish(&mut font);
    assert_eq!(t.semantic(1).unwrap().language(), Some("zh-CN"));
    assert_eq!(t.semantic(FIRST).unwrap().language(), Some("en-US"));
}

#[test]
fn headings_have_levels_and_name_their_section() {
    let t = form(false, &browsers(), None, 600.0);
    assert_eq!(t.semantic(1).unwrap().role(), Role::Window);
    assert_eq!(t.semantic(2).unwrap().level(), Some(1));
    assert_eq!(t.semantic(3).unwrap().level(), Some(2));
    let card = t.semantic(4).unwrap();
    assert_eq!(card.role(), Role::Group);
    assert_eq!(card.labelled_by(), &[NodeId(3)]);
}

#[test]
fn errors_are_assertive_and_other_news_polite() {
    let mut font = Font::for_test();
    let mut f = Form::new(1, "Settings", "en-US", 600.0);
    f.status(5, "Could not save the setting", true);
    f.status(6, "Saved", false);
    let t = f.finish(&mut font);
    assert_eq!(t.semantic(5).unwrap().role(), Role::Status);
    assert_eq!(t.semantic(5).unwrap().live(), Some(Live::Assertive));
    assert_eq!(t.semantic(6).unwrap().live(), Some(Live::Polite));
}

fn long_form() -> Tree {
    let mut font = Font::for_test();
    let mut f = Form::new(1, "Settings", "en-US", 400.0);
    for i in 0..8 {
        let mut s = f.section(100 + i, 200 + i, "Section");
        f.info(&mut s, 300 + i, "Version", "1");
        f.info(&mut s, 400 + i, "System", "Debian");
    }
    f.finish(&mut font)
}

#[test]
fn a_tall_form_has_a_scrollbar_tools_can_read_and_set() {
    let mut t = long_form();
    let view = Rect::new(0.0, 0.0, 400.0, 300.0);
    let max = t.size().1 - 300.0;
    assert!(max > 300.0, "{max}");
    let (nodes, _) = t.accesskit(view, (10, 20));
    let bar = nodes
        .iter()
        .find(|(id, _)| id.0 == t.scrollbar_id())
        .map(|(_, n)| n)
        .unwrap();
    assert_eq!(bar.role(), Role::ScrollBar);
    assert_eq!(bar.max_numeric_value(), Some(max as f64));
    assert_eq!(bar.numeric_value(), Some(0.0));
    assert_eq!(bar.controls(), &[NodeId(1)]);
    let root = nodes
        .iter()
        .find(|(id, _)| id.0 == 1)
        .map(|(_, n)| n)
        .unwrap();
    assert_eq!(root.scroll_y_max(), Some(max as f64));
    assert!(root.children().contains(&NodeId(t.scrollbar_id())));

    let x = 400.0 - 6.0;
    let Some(BarHit::Thumb(grab)) = t.bar_hit(view, x, 5.0) else {
        panic!("the thumb is at the top");
    };
    assert_eq!(t.bar_hit(view, x, 290.0), Some(BarHit::Track(true)));
    assert_eq!(t.bar_hit(view, 200.0, 5.0), None);
    t.drag_thumb(view, 1000.0, grab);
    assert_eq!(t.scroll, max);
    assert_eq!(t.bar_hit(view, x, 5.0), Some(BarHit::Track(false)));
    t.drag_thumb(view, -50.0, grab);
    assert_eq!(t.scroll, 0.0);
}

#[test]
fn a_short_form_has_no_scrollbar() {
    let t = form(false, &browsers(), None, 600.0);
    let view = Rect::new(0.0, 0.0, 600.0, 2000.0);
    assert!(t.bar_hit(view, 594.0, 5.0).is_none());
    let (nodes, _) = t.accesskit(view, (0, 0));
    assert!(nodes.iter().all(|(_, n)| n.role() != Role::ScrollBar));
}

fn focus_pixels(t: &Tree) -> usize {
    let mut font = Font::for_test();
    let (w, h) = (600, t.size().1.ceil() as usize);
    let mut c = Canvas::new(w, h, theme().bg);
    t.paint(
        &mut c,
        &mut Paint {
            font: &mut font,
            icons: &|_| None,
            field: None,
        },
        Rect::new(0.0, 0.0, w as f32, h as f32),
    );
    let ring = theme().controls.focus;
    c.px.iter().filter(|&&p| p == ring).count()
}

#[test]
fn the_focus_ring_shows_for_the_keyboard_only() {
    for dark in [false, true] {
        crate::draw::with_theme(dark, || {
            let mut t = form(false, &browsers(), None, 600.0);
            assert_eq!(focus_pixels(&t), 0);
            t.step(true, 2000.0);
            t.step(true, 2000.0);
            assert!(focus_pixels(&t) > 100);
            t.pointer = true;
            assert_eq!(focus_pixels(&t), 0);
        });
    }
}
