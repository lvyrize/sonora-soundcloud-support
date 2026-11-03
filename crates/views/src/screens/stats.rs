use std::time::Duration;

use gpui::accesskit::Role;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Pixels, Render, ScrollHandle, SharedString,
    Window, div, px, relative,
};
use i18n::t;
use music::Track;
use state::{Playback, Ranked, Report, Sonora, Span, Stats, StatsState, Step};
use ui::{
    ActiveTheme as _, Button, InfoCard, Listing as _, Room, Scrollbar, Scroller, TabBar,
    TableDelegate, TableEvent, TableState, Text, table, tabular, vacant,
};

use crate::shared::album_grid::CardGrid;
use crate::shared::cards;
use crate::shared::cells;
use crate::shared::hero::{HeroMetaStrip, PageHero};
use crate::shared::page;
use crate::shared::tracks::{STATS_COLUMNS, TrackSource, Tracks};

/// How tall a chart's bars may grow.
const CHART: Pixels = px(120.);
/// The space between two bars of a chart.
const BAR_GAP: Pixels = px(2.);
/// The least a bar with anything in it shows, so a short listen is still seen.
const BAR_FLOOR: f32 = 0.04;
/// The spans offered before the earlier years, in the order the tabs show them.
const SPANS: [Span; 7] = [
    Span::Today,
    Span::Week,
    Span::FourWeeks,
    Span::Month,
    Span::SixMonths,
    Span::ThisYear,
    Span::Lifetime,
];

/// Which ranking the lower half of the page shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Songs,
    Artists,
    Albums,
}

impl Section {
    const ALL: [Self; 3] = [Self::Songs, Self::Artists, Self::Albums];

    fn label(self) -> SharedString {
        match self {
            Self::Songs => t!("stats-top-songs"),
            Self::Artists => t!("stats-top-artists"),
            Self::Albums => t!("stats-top-albums"),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Songs => "stats-section-songs",
            Self::Artists => "stats-section-artists",
            Self::Albums => "stats-section-albums",
        }
    }
}

struct TopTracks(Entity<Stats>);

impl Tracks for TopTracks {
    fn tracks<'a>(&self, cx: &'a App) -> &'a [Track] {
        &self.0.read(cx).report().top_tracks
    }

    fn is_loading(&self, cx: &App) -> bool {
        self.0.read(cx).state() == StatsState::Loading
    }
}

/// The listening stats page: what was heard over a span, the top songs, artists and albums,
/// and when the listening happened.
pub(crate) struct StatsView {
    stats: Entity<Stats>,
    playback: Entity<Playback>,
    width: Pixels,
    scrollbar: Entity<Scrollbar>,
    table: Entity<TableState<TrackSource>>,
    section: Section,
}

impl StatsView {
    pub(crate) fn new(
        stats: Entity<Stats>,
        playback: Entity<Playback>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let width = cells::content_width(window, Pixels::ZERO, cx);
        let id = cx.entity_id();
        let scrollbar = cx.new(|_| Scrollbar::new(ScrollHandle::new()).watching(id));
        let scroll = scrollbar.read(cx).scroll().clone();
        let table = cx.new(|cx| {
            let menu = cx.new(|_| Scrollbar::inset().watching(id));
            crate::shared::tracks::follow_downloads(cx);
            let source = TrackSource::new(
                STATS_COLUMNS,
                TopTracks(stats.clone()),
                playback.clone(),
                menu,
                cx,
            )
            .with_liked(Sonora::global(cx).library.clone())
            .table(cx.weak_entity());
            TableState::new(TableDelegate::new(source, width, cx), cx).follow(scroll)
        });

        cx.observe(&stats, |this, _, cx| {
            this.table.rebuild(cx);
            cx.notify();
        })
        .detach();
        cx.observe(&playback, |this, _, cx| {
            this.table.refresh(cx);
            cx.notify();
        })
        .detach();
        let library = Sonora::global(cx).library.clone();
        cx.observe(&library, |this, _, cx| {
            this.table.refresh(cx);
            cx.notify();
        })
        .detach();
        cx.subscribe(&table, |this, _, event, cx| match event {
            TableEvent::DoubleClicked(display) => {
                page::play(&this.table, &this.playback, *display, cx);
            }
            TableEvent::Activated(display) => {
                page::play_or_toggle(&this.table, &this.playback, *display, cx);
            }
            _ => {}
        })
        .detach();

        Self {
            stats,
            playback,
            width,
            scrollbar,
            table,
            section: Section::Songs,
        }
    }

    /// Works the stats out again, as the page opens or is asked to reload.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.stats.update(cx, |stats, cx| stats.refresh(cx));
    }

    fn header(&self, report: &Report, span: Span) -> AnyElement {
        let mut strip = HeroMetaStrip::new()
            .text(t!("stats-minutes", count = minutes(report.listened)))
            .text(t!("stats-streams", count = report.streams));
        if report.tracks > 0 {
            strip = strip.text(t!("count-songs", count = report.tracks));
        }
        PageHero::new("stats-hero", t!("nav-stats"))
            .fallback("icons/chart-column.svg")
            .accent()
            .eyebrow(span_label(span))
            .meta(strip)
            .into_any_element()
    }

    /// The span tabs: the fixed spans, then every earlier year that has listens.
    fn spans(&self, cx: &mut Context<Self>) -> AnyElement {
        let stats = self.stats.read(cx);
        let chosen = stats.span();
        let spans: Vec<Span> = SPANS
            .into_iter()
            .chain(stats.years().iter().map(|year| Span::Year(*year)))
            .collect();
        let count = spans.len();
        let tabs = spans.into_iter().enumerate().map(|(index, span)| {
            Button::new(SharedString::from(format!("stats-span-{}", span_id(span))))
                .label(span_label(span))
                .small()
                .ghost()
                .selected(span == chosen)
                .aria_position_in_set(index + 1)
                .aria_size_of_set(count)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.stats.update(cx, |stats, cx| stats.set_span(span, cx));
                }))
        });
        TabBar::new("stats-spans")
            .name(t!("stats-spans"))
            .items(tabs.collect::<Vec<_>>())
            .into_any_element()
    }

    /// The totals for the span, one tile each.
    fn tiles(&self, report: &Report, cx: &App) -> AnyElement {
        let theme = *cx.theme();
        let tiles = [
            (
                "stats-tile-minutes",
                t!("stats-tile-minutes"),
                minutes(report.listened),
            ),
            (
                "stats-tile-streams",
                t!("stats-tile-streams"),
                report.streams,
            ),
            (
                "stats-tile-songs",
                t!("stats-tile-songs"),
                report.tracks as u64,
            ),
            (
                "stats-tile-artists",
                t!("stats-tile-artists"),
                report.artists as u64,
            ),
            (
                "stats-tile-albums",
                t!("stats-tile-albums"),
                report.albums as u64,
            ),
        ];
        div()
            .flex()
            .flex_wrap()
            .gap(theme.metrics.pad)
            .children(tiles.into_iter().map(|(id, label, value)| {
                let value = cells::count(value);
                div()
                    .id(id)
                    .role(Role::Group)
                    .aria_label(format!("{label}: {value}"))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(theme.metrics.cover)
                    .gap_1()
                    .p(theme.metrics.pad)
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .child(
                        div()
                            .text_size(theme.text(Text::Small))
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(theme.text(Text::Title))
                            .font_weight(FontWeight::BOLD)
                            .font_features(tabular())
                            .child(value),
                    )
            }))
            .into_any_element()
    }

    /// The listening clock and the activity over the span, side by side when there is room.
    fn charts(&self, report: &Report, cx: &App) -> AnyElement {
        let theme = *cx.theme();
        let clock: Vec<Duration> = report.clock.to_vec();
        let peak = clock
            .iter()
            .enumerate()
            .max_by_key(|(_, heard)| **heard)
            .filter(|(_, heard)| !heard.is_zero())
            .map(|(hour, _)| hour);
        let clock_summary = match peak {
            Some(hour) => {
                let hour = hour_label(hour);
                t!("stats-clock-peak", hour = &hour)
            }
            None => t!("stats-nothing"),
        };
        let clock_labels = [0usize, 6, 12, 18].map(hour_label).to_vec();
        let clock_chart = InfoCard::new(t!("stats-clock"))
            .spoken("stats-clock")
            .stretch()
            .child(bars(
                "stats-clock-bars",
                &clock,
                clock_summary,
                clock_labels,
                cx,
            ));

        let heard: Vec<Duration> = report
            .activity
            .iter()
            .map(|bucket| bucket.listened)
            .collect();
        let step = report.step.unwrap_or(Step::Day);
        let ends: Vec<SharedString> = match (report.activity.first(), report.activity.last()) {
            (Some(first), Some(last)) if report.activity.len() > 1 => vec![
                bucket_label(first.start, step),
                bucket_label(last.start, step),
            ],
            (Some(only), _) => vec![bucket_label(only.start, step)],
            _ => Vec::new(),
        };
        let activity_summary = t!("stats-minutes", count = minutes(report.listened));
        let activity_chart = InfoCard::new(t!("stats-activity"))
            .spoken("stats-activity")
            .stretch()
            .child(bars(
                "stats-activity-bars",
                &heard,
                activity_summary,
                ends,
                cx,
            ));

        let stacked = matches!(Room::of(self.width), Room::Tight | Room::Snug);
        div()
            .flex()
            .gap(theme.metrics.pad)
            .map(|this| match stacked {
                true => this.flex_col(),
                false => this.flex_row(),
            })
            .child(div().flex_1().min_w_0().child(clock_chart))
            .child(div().flex_1().min_w_0().child(activity_chart))
            .into_any_element()
    }

    /// The tabs that pick a ranking.
    fn sections(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = Section::ALL.len();
        let tabs = Section::ALL
            .into_iter()
            .enumerate()
            .map(|(index, section)| {
                Button::new(section.id())
                    .label(section.label())
                    .small()
                    .ghost()
                    .selected(section == self.section)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(count)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        cx.notify();
                    }))
            });
        TabBar::new("stats-sections")
            .name(t!("stats-sections"))
            .items(tabs.collect::<Vec<_>>())
            .into_any_element()
    }

    fn artists(&self, ranked: &[Ranked<music::SavedArtist>], cx: &App) -> AnyElement {
        let layout = CardGrid::layout(self.width);
        let cards = ranked.iter().enumerate().map(|(index, ranked)| {
            let line = ranked_line(ranked.streams, ranked.listened);
            cards::artist_card(("stats-artist", index), &ranked.item, &self.playback, cx)
                .tile(layout.card)
                .flat()
                .eyebrow(t!("stats-rank", rank = index + 1))
                .aria_description(line.clone())
                .meta(line)
                .into_any_element()
        });
        grid(layout.gap, cards)
    }

    fn albums(&self, ranked: &[Ranked<music::Album>], cx: &App) -> AnyElement {
        let layout = CardGrid::layout(self.width);
        let cards = ranked.iter().enumerate().map(|(index, ranked)| {
            let line = ranked_line(ranked.streams, ranked.listened);
            cards::album_card(("stats-album", index), &ranked.item, &self.playback, cx)
                .tile(layout.card)
                .flat()
                .eyebrow(t!("stats-rank-streams", rank = index + 1, line = &line))
                .aria_description(line)
                .into_any_element()
        });
        grid(layout.gap, cards)
    }

    fn note(&self, cx: &App) -> Option<SharedString> {
        let stats = self.stats.read(cx);
        match stats.state() {
            StatsState::Loading => None,
            StatsState::Failed => Some(t!("stats-failed")),
            StatsState::Ready if stats.report().streams == 0 => Some(t!("stats-empty")),
            StatsState::Ready => None,
        }
    }
}

impl Render for StatsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.table.claim(cx);
        let inset = cx.theme().metrics.inset;
        let gap = cx.theme().metrics.pad;
        let width = cells::content_width(window, Pixels::ZERO, cx);
        if (width - self.width).abs() >= px(0.5) {
            self.width = width;
            self.table.set_width(width, cx);
        }
        let scroll = self.scrollbar.read(cx).scroll().clone();
        let viewport = page::viewport(&scroll, inset, window);
        self.table
            .update(cx, |table, _| table.set_viewport(viewport));

        let stats = self.stats.read(cx);
        let report = stats.report().clone();
        let span = stats.span();
        let note = self.note(cx);
        let empty = note.is_some();
        let header = self.header(&report, span);
        let spans = self.spans(cx);
        let tiles = self.tiles(&report, cx);
        let charts = self.charts(&report, cx);
        let sections = self.sections(cx);
        let ranking = match self.section {
            Section::Songs => None,
            Section::Artists => Some(self.artists(&report.top_artists, cx)),
            Section::Albums => Some(self.albums(&report.top_albums, cx)),
        };

        let page = Scroller::new("stats-page", &self.scrollbar)
            .bottom_inset(crate::chrome::Chrome::bottom(cx))
            .pt(inset)
            .pb(inset)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(gap)
                    .px(inset)
                    .child(header)
                    .child(spans)
                    .when(!empty, |this| {
                        this.child(tiles).child(charts).child(sections)
                    }),
            )
            .when_some(note, |this, note| {
                this.child(
                    div()
                        .id("stats-empty")
                        .role(Role::Status)
                        .aria_label(note.clone())
                        .child(vacant(note, cx)),
                )
            })
            .when(!empty, |this| match ranking {
                None => this.child(table(&self.table)),
                Some(ranking) => this.child(div().px(inset).pt(gap).child(ranking)),
            });

        div().size_full().child(page)
    }
}

/// A bar chart of `values`, each bar as tall as its share of the largest, with `labels` spread
/// evenly underneath. Screen readers hear `summary`, since the bars themselves say nothing.
fn bars(
    id: &'static str,
    values: &[Duration],
    summary: SharedString,
    labels: Vec<SharedString>,
    cx: &App,
) -> AnyElement {
    let theme = *cx.theme();
    let most = values.iter().max().copied().unwrap_or_default();
    let shares = values.iter().map(|value| match most.is_zero() {
        true => 0.,
        false => {
            let share = value.as_secs_f32() / most.as_secs_f32();
            match value.is_zero() {
                true => 0.,
                false => share.max(BAR_FLOOR),
            }
        }
    });
    div()
        .id(id)
        .role(Role::Figure)
        .aria_label(summary)
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_end()
                .gap(BAR_GAP)
                .h(CHART)
                .children(shares.map(|share| {
                    div()
                        .flex_1()
                        .h(relative(share))
                        .rounded_t(theme.radius / 2.)
                        .bg(theme.primary)
                })),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .text_size(theme.text(Text::Tiny))
                .text_color(theme.muted_foreground)
                .font_features(tabular())
                .children(labels),
        )
        .into_any_element()
}

/// Cards laid out in rows that wrap, as the library's album grid does.
fn grid(gap: Pixels, cards: impl Iterator<Item = AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .w_full()
        .gap_x(gap)
        .gap_y_6()
        .children(cards)
        .into_any_element()
}

/// "12 streams · 40 min", for a ranked artist or album.
fn ranked_line(streams: u64, listened: Duration) -> SharedString {
    let streams = t!("stats-streams", count = streams);
    let minutes = t!("stats-minutes", count = minutes(listened));
    t!("stats-ranked", streams = &streams, minutes = &minutes)
}

fn minutes(listened: Duration) -> u64 {
    listened.as_secs() / 60
}

fn span_id(span: Span) -> String {
    match span {
        Span::Today => "today".into(),
        Span::Week => "week".into(),
        Span::FourWeeks => "four-weeks".into(),
        Span::Month => "month".into(),
        Span::SixMonths => "six-months".into(),
        Span::ThisYear => "this-year".into(),
        Span::Year(year) => year.to_string(),
        Span::Lifetime => "lifetime".into(),
    }
}

fn span_label(span: Span) -> SharedString {
    match span {
        Span::Today => t!("stats-span-today"),
        Span::Week => t!("stats-span-week"),
        Span::FourWeeks => t!("stats-span-four-weeks"),
        Span::Month => t!("stats-span-month"),
        Span::SixMonths => t!("stats-span-six-months"),
        Span::ThisYear => t!("stats-span-this-year"),
        Span::Year(year) => SharedString::from(year.to_string()),
        Span::Lifetime => t!("stats-span-lifetime"),
    }
}

fn hour_label(hour: usize) -> SharedString {
    t!("stats-hour", hour = format!("{hour:02}"))
}

/// Names the bar of the activity chart that starts at `start`, in seconds since the epoch.
fn bucket_label(start: i64, step: Step) -> SharedString {
    let Ok(stamp) = jiff::Timestamp::new(start, 0) else {
        return SharedString::default();
    };
    let at = stamp.to_zoned(jiff::tz::TimeZone::system());
    let month = month_label(at.month());
    match step {
        Step::Hour => hour_label(at.hour().max(0) as usize),
        Step::Day | Step::Week => t!("stats-day", month = &month, day = at.day()),
        Step::Month => t!("stats-month", month = &month, year = at.year()),
    }
}

fn month_label(month: i8) -> SharedString {
    match month {
        1 => t!("month-1"),
        2 => t!("month-2"),
        3 => t!("month-3"),
        4 => t!("month-4"),
        5 => t!("month-5"),
        6 => t!("month-6"),
        7 => t!("month-7"),
        8 => t!("month-8"),
        9 => t!("month-9"),
        10 => t!("month-10"),
        11 => t!("month-11"),
        _ => t!("month-12"),
    }
}
