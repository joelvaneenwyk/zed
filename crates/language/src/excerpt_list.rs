use crate::{buffer, Buffer, Chunk};
use collections::HashMap;
use gpui::{AppContext, Entity, ModelContext, ModelHandle};
use parking_lot::Mutex;
use smallvec::{smallvec, SmallVec};
use std::{cmp, iter, ops::Range};
use sum_tree::{Bias, Cursor, SumTree};
use text::{
    subscription::{Subscription, Topic},
    Anchor, AnchorRangeExt, Edit, TextSummary,
};
use theme::SyntaxTheme;

const NEWLINES: &'static [u8] = &[b'\n'; u8::MAX as usize];

pub trait ToOffset {
    fn to_offset<'a>(&self, content: &Snapshot) -> usize;
}

pub type ExcerptId = Location;

#[derive(Default)]
pub struct ExcerptList {
    snapshot: Mutex<Snapshot>,
    buffers: HashMap<usize, BufferState>,
    subscriptions: Topic,
}

#[derive(Debug)]
struct BufferState {
    buffer: ModelHandle<Buffer>,
    last_sync: clock::Global,
    excerpts: Vec<ExcerptId>,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    excerpts: SumTree<Excerpt>,
}

pub struct ExcerptProperties<'a, T> {
    buffer: &'a ModelHandle<Buffer>,
    range: Range<T>,
    header_height: u8,
}

#[derive(Clone)]
struct Excerpt {
    id: ExcerptId,
    buffer: buffer::Snapshot,
    range: Range<Anchor>,
    text_summary: TextSummary,
    header_height: u8,
}

#[derive(Clone, Debug, Default)]
struct EntrySummary {
    excerpt_id: ExcerptId,
    text: TextSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Location(SmallVec<[u8; 4]>);

pub struct Chunks<'a> {
    range: Range<usize>,
    cursor: Cursor<'a, Excerpt, usize>,
    header_height: u8,
    entry_chunks: Option<buffer::Chunks<'a>>,
    theme: Option<&'a SyntaxTheme>,
}

impl ExcerptList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self, cx: &AppContext) -> Snapshot {
        self.sync(cx);
        self.snapshot.lock().clone()
    }

    pub fn subscribe(&mut self) -> Subscription {
        self.subscriptions.subscribe()
    }

    pub fn push<O>(&mut self, props: ExcerptProperties<O>, cx: &mut ModelContext<Self>) -> ExcerptId
    where
        O: text::ToOffset,
    {
        self.sync(cx);

        let buffer = props.buffer.read(cx);
        let range = buffer.anchor_before(props.range.start)..buffer.anchor_after(props.range.end);
        let mut snapshot = self.snapshot.lock();
        let prev_id = snapshot.excerpts.last().map(|e| &e.id);
        let id = ExcerptId::between(prev_id.unwrap_or(&ExcerptId::min()), &ExcerptId::max());

        let edit_start = snapshot.excerpts.summary().text.bytes;
        let excerpt = Excerpt::new(id.clone(), buffer.snapshot(), range, props.header_height);
        let edit = Edit {
            old: edit_start..edit_start,
            new: edit_start..edit_start + excerpt.text_summary.bytes,
        };
        snapshot.excerpts.push(excerpt, &());
        self.buffers
            .entry(props.buffer.id())
            .or_insert_with(|| BufferState {
                buffer: props.buffer.clone(),
                last_sync: buffer.version(),
                excerpts: Default::default(),
            })
            .excerpts
            .push(id.clone());

        self.subscriptions.publish_mut([edit]);

        id
    }

    fn sync(&self, cx: &AppContext) {
        let mut snapshot = self.snapshot.lock();
        let mut excerpts_to_edit = Vec::new();
        for buffer_state in self.buffers.values() {
            if buffer_state
                .buffer
                .read(cx)
                .version()
                .gt(&buffer_state.last_sync)
            {
                excerpts_to_edit.extend(
                    buffer_state
                        .excerpts
                        .iter()
                        .map(|excerpt_id| (excerpt_id, buffer_state)),
                );
            }
        }
        excerpts_to_edit.sort_unstable_by_key(|(excerpt_id, _)| *excerpt_id);

        let mut edits = Vec::new();
        let mut new_excerpts = SumTree::new();
        let mut cursor = snapshot.excerpts.cursor::<(ExcerptId, usize)>();

        for (id, buffer_state) in excerpts_to_edit {
            new_excerpts.push_tree(cursor.slice(id, Bias::Left, &()), &());
            let old_excerpt = cursor.item().unwrap();
            let buffer = buffer_state.buffer.read(cx);

            edits.extend(
                buffer
                    .edits_since_in_range::<usize>(
                        old_excerpt.buffer.version(),
                        old_excerpt.range.clone(),
                    )
                    .map(|mut edit| {
                        let excerpt_old_start =
                            cursor.start().1 + old_excerpt.header_height as usize;
                        let excerpt_new_start =
                            new_excerpts.summary().text.bytes + old_excerpt.header_height as usize;
                        edit.old.start += excerpt_old_start;
                        edit.old.end += excerpt_old_start;
                        edit.new.start += excerpt_new_start;
                        edit.new.end += excerpt_new_start;
                        edit
                    }),
            );

            new_excerpts.push(
                Excerpt::new(
                    id.clone(),
                    buffer.snapshot(),
                    old_excerpt.range.clone(),
                    old_excerpt.header_height,
                ),
                &(),
            );

            cursor.next(&());
        }
        new_excerpts.push_tree(cursor.suffix(&()), &());

        drop(cursor);
        snapshot.excerpts = new_excerpts;

        self.subscriptions.publish(edits);
    }
}

impl Entity for ExcerptList {
    type Event = ();
}

impl Snapshot {
    pub fn text(&self) -> String {
        self.chunks(0..self.len(), None)
            .map(|chunk| chunk.text)
            .collect()
    }

    pub fn text_for_range<'a, T: ToOffset>(
        &'a self,
        range: Range<T>,
    ) -> impl Iterator<Item = &'a str> {
        self.chunks(range, None).map(|chunk| chunk.text)
    }

    pub fn len(&self) -> usize {
        self.excerpts.summary().text.bytes
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        let mut cursor = self.excerpts.cursor::<usize>();
        cursor.seek(&offset, bias, &());
        if let Some(excerpt) = cursor.item() {
            let overshoot = offset - cursor.start();
            let header_height = excerpt.header_height as usize;
            if overshoot < header_height {
                *cursor.start()
            } else {
                let excerpt_start =
                    text::ToOffset::to_offset(&excerpt.range.start, &excerpt.buffer);
                let buffer_offset = excerpt.buffer.clip_offset(
                    excerpt_start + (offset - header_height - cursor.start()),
                    bias,
                );
                let offset_in_excerpt = if buffer_offset > excerpt_start {
                    buffer_offset - excerpt_start
                } else {
                    0
                };
                cursor.start() + header_height + offset_in_excerpt
            }
        } else {
            self.excerpts.summary().text.bytes
        }
    }

    pub fn chunks<'a, T: ToOffset>(
        &'a self,
        range: Range<T>,
        theme: Option<&'a SyntaxTheme>,
    ) -> Chunks<'a> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let mut cursor = self.excerpts.cursor::<usize>();
        cursor.seek(&range.start, Bias::Right, &());

        let mut header_height: u8 = 0;
        let entry_chunks = cursor.item().map(|excerpt| {
            let buffer_range = excerpt.range.to_offset(&excerpt.buffer);
            header_height = excerpt.header_height;

            let buffer_start;
            let start_overshoot = range.start - cursor.start();
            if start_overshoot < excerpt.header_height as usize {
                header_height -= start_overshoot as u8;
                buffer_start = buffer_range.start;
            } else {
                buffer_start =
                    buffer_range.start + start_overshoot - excerpt.header_height as usize;
                header_height = 0;
            }

            let buffer_end;
            let end_overshoot = range.end - cursor.start();
            if end_overshoot < excerpt.header_height as usize {
                header_height -= excerpt.header_height - end_overshoot as u8;
                buffer_end = buffer_start;
            } else {
                buffer_end = cmp::min(
                    buffer_range.end,
                    buffer_range.start + end_overshoot - excerpt.header_height as usize,
                );
            }

            excerpt.buffer.chunks(buffer_start..buffer_end, theme)
        });

        Chunks {
            range,
            cursor,
            header_height,
            entry_chunks,
            theme,
        }
    }
}

impl Excerpt {
    fn new(
        id: ExcerptId,
        buffer: buffer::Snapshot,
        range: Range<Anchor>,
        header_height: u8,
    ) -> Self {
        let mut text_summary = buffer.text_summary_for_range::<TextSummary, _>(range.clone());
        if header_height > 0 {
            text_summary.first_line_chars = 0;
            text_summary.lines.row += header_height as u32;
            text_summary.lines_utf16.row += header_height as u32;
            text_summary.bytes += header_height as usize;
            text_summary.longest_row += header_height as u32;
        }
        text_summary.last_line_chars = 0;
        text_summary.lines.row += 1;
        text_summary.lines.column = 0;
        text_summary.lines_utf16.row += 1;
        text_summary.lines_utf16.column = 0;
        text_summary.bytes += 1;

        Excerpt {
            id,
            buffer,
            range,
            text_summary,
            header_height,
        }
    }
}

impl sum_tree::Item for Excerpt {
    type Summary = EntrySummary;

    fn summary(&self) -> Self::Summary {
        EntrySummary {
            excerpt_id: self.id.clone(),
            text: self.text_summary.clone(),
        }
    }
}

impl sum_tree::Summary for EntrySummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, _: &()) {
        debug_assert!(summary.excerpt_id > self.excerpt_id);
        self.excerpt_id = summary.excerpt_id.clone();
        self.text.add_summary(&summary.text, &());
    }
}

impl<'a> sum_tree::Dimension<'a, EntrySummary> for usize {
    fn add_summary(&mut self, summary: &'a EntrySummary, _: &()) {
        *self += summary.text.bytes;
    }
}

impl<'a> sum_tree::Dimension<'a, EntrySummary> for Location {
    fn add_summary(&mut self, summary: &'a EntrySummary, _: &()) {
        debug_assert!(summary.excerpt_id > *self);
        *self = summary.excerpt_id.clone();
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.header_height > 0 {
            let chunk = Chunk {
                text: unsafe {
                    std::str::from_utf8_unchecked(&NEWLINES[..self.header_height as usize])
                },
                ..Default::default()
            };
            self.header_height = 0;
            return Some(chunk);
        }

        if let Some(entry_chunks) = self.entry_chunks.as_mut() {
            if let Some(chunk) = entry_chunks.next() {
                return Some(chunk);
            } else if self.range.end >= self.cursor.end(&()) {
                self.entry_chunks.take();
                return Some(Chunk {
                    text: "\n",
                    ..Default::default()
                });
            } else {
                return None;
            }
        }

        self.cursor.next(&());
        if *self.cursor.start() == self.range.end {
            return None;
        }

        let excerpt = self.cursor.item()?;
        let buffer_range = excerpt.range.to_offset(&excerpt.buffer);

        let buffer_end = cmp::min(
            buffer_range.end,
            buffer_range.start + self.range.end
                - excerpt.header_height as usize
                - self.cursor.start(),
        );

        self.header_height = excerpt.header_height;
        self.entry_chunks = Some(
            excerpt
                .buffer
                .chunks(buffer_range.start..buffer_end, self.theme),
        );

        self.next()
    }
}

impl ToOffset for usize {
    fn to_offset<'a>(&self, _: &Snapshot) -> usize {
        *self
    }
}

impl Default for Location {
    fn default() -> Self {
        Self::min()
    }
}

impl Location {
    pub fn min() -> Self {
        Self(smallvec![u8::MIN])
    }

    pub fn max() -> Self {
        Self(smallvec![u8::MAX])
    }

    pub fn between(lhs: &Self, rhs: &Self) -> Self {
        let lhs = lhs.0.iter().copied().chain(iter::repeat(u8::MIN));
        let rhs = rhs.0.iter().copied().chain(iter::repeat(u8::MAX));
        let mut location = SmallVec::new();
        for (lhs, rhs) in lhs.zip(rhs) {
            let mid = lhs + (rhs.saturating_sub(lhs)) / 2;
            location.push(mid);
            if mid > lhs {
                break;
            }
        }
        Self(location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;
    use gpui::MutableAppContext;
    use rand::prelude::*;
    use std::{env, mem};
    use text::{Point, RandomCharIter};
    use util::test::sample_text;

    #[gpui::test]
    fn test_excerpt_buffer(cx: &mut MutableAppContext) {
        let buffer_1 = cx.add_model(|cx| Buffer::new(0, sample_text(6, 6, 'a'), cx));
        let buffer_2 = cx.add_model(|cx| Buffer::new(0, sample_text(6, 6, 'g'), cx));

        let list = cx.add_model(|_| ExcerptList::new());

        let subscription = list.update(cx, |list, cx| {
            let subscription = list.subscribe();
            list.push(
                ExcerptProperties {
                    buffer: &buffer_1,
                    range: Point::new(1, 2)..Point::new(2, 5),
                    header_height: 2,
                },
                cx,
            );
            assert_eq!(
                subscription.consume().into_inner(),
                [Edit {
                    old: 0..0,
                    new: 0..13
                }]
            );

            list.push(
                ExcerptProperties {
                    buffer: &buffer_1,
                    range: Point::new(3, 3)..Point::new(4, 4),
                    header_height: 1,
                },
                cx,
            );
            list.push(
                ExcerptProperties {
                    buffer: &buffer_2,
                    range: Point::new(3, 1)..Point::new(3, 3),
                    header_height: 3,
                },
                cx,
            );
            assert_eq!(
                subscription.consume().into_inner(),
                [Edit {
                    old: 13..13,
                    new: 13..29
                }]
            );

            subscription
        });

        assert_eq!(
            list.read(cx).snapshot(cx).text(),
            concat!(
                "\n",      // Preserve newlines
                "\n",      //
                "bbbb\n",  //
                "ccccc\n", //
                "\n",      //
                "ddd\n",   //
                "eeee\n",  //
                "\n",      //
                "\n",      //
                "\n",      //
                "jj\n"     //
            )
        );

        buffer_1.update(cx, |buffer, cx| {
            buffer.edit(
                [
                    Point::new(0, 0)..Point::new(0, 0),
                    Point::new(2, 1)..Point::new(2, 3),
                ],
                "\n",
                cx,
            );
        });

        assert_eq!(
            list.read(cx).snapshot(cx).text(),
            concat!(
                "\n",     // Preserve newlines
                "\n",     //
                "bbbb\n", //
                "c\n",    //
                "cc\n",   //
                "\n",     //
                "ddd\n",  //
                "eeee\n", //
                "\n",     //
                "\n",     //
                "\n",     //
                "jj\n"    //
            )
        );

        assert_eq!(
            subscription.consume().into_inner(),
            [Edit {
                old: 18..20,
                new: 18..19
            }]
        );
    }

    #[gpui::test(iterations = 100)]
    fn test_random_excerpts(cx: &mut MutableAppContext, mut rng: StdRng) {
        let operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);

        let mut buffers: Vec<ModelHandle<Buffer>> = Vec::new();
        let list = cx.add_model(|_| ExcerptList::new());
        let mut excerpt_ids = Vec::new();
        let mut expected_excerpts = Vec::new();
        let mut old_versions = Vec::new();

        for _ in 0..operations {
            match rng.gen_range(0..100) {
                0..=19 if !buffers.is_empty() => {
                    let buffer = buffers.choose(&mut rng).unwrap();
                    buffer.update(cx, |buf, cx| buf.randomly_edit(&mut rng, 1, cx));
                }
                _ => {
                    let buffer_handle = if buffers.is_empty() || rng.gen_bool(0.4) {
                        let base_text = RandomCharIter::new(&mut rng).take(10).collect::<String>();
                        buffers.push(cx.add_model(|cx| Buffer::new(0, base_text, cx)));
                        buffers.last().unwrap()
                    } else {
                        buffers.choose(&mut rng).unwrap()
                    };

                    let buffer = buffer_handle.read(cx);
                    let end_ix = buffer.clip_offset(rng.gen_range(0..=buffer.len()), Bias::Right);
                    let start_ix = buffer.clip_offset(rng.gen_range(0..=end_ix), Bias::Left);
                    let header_height = rng.gen_range(0..=5);
                    let anchor_range = buffer.anchor_before(start_ix)..buffer.anchor_after(end_ix);
                    log::info!(
                        "Pushing excerpt wih header {}, buffer {}: {:?}[{:?}] = {:?}",
                        header_height,
                        buffer_handle.id(),
                        buffer.text(),
                        start_ix..end_ix,
                        &buffer.text()[start_ix..end_ix]
                    );

                    let excerpt_id = list.update(cx, |list, cx| {
                        list.push(
                            ExcerptProperties {
                                buffer: &buffer_handle,
                                range: start_ix..end_ix,
                                header_height,
                            },
                            cx,
                        )
                    });
                    excerpt_ids.push(excerpt_id);
                    expected_excerpts.push((buffer_handle.clone(), anchor_range, header_height));
                }
            }

            if rng.gen_bool(0.3) {
                list.update(cx, |list, cx| {
                    old_versions.push((list.snapshot(cx), list.subscribe()));
                })
            }

            let snapshot = list.read(cx).snapshot(cx);
            let mut expected_text = String::new();
            for (buffer, range, header_height) in &expected_excerpts {
                let buffer = buffer.read(cx);
                for _ in 0..*header_height {
                    expected_text.push('\n');
                }
                expected_text.extend(buffer.text_for_range(range.clone()));
                expected_text.push('\n');
            }
            assert_eq!(snapshot.text(), expected_text);

            for _ in 0..10 {
                let end_ix = snapshot.clip_offset(rng.gen_range(0..=snapshot.len()), Bias::Right);
                let start_ix = snapshot.clip_offset(rng.gen_range(0..=end_ix), Bias::Left);

                assert_eq!(
                    snapshot
                        .text_for_range(start_ix..end_ix)
                        .collect::<String>(),
                    &expected_text[start_ix..end_ix],
                    "incorrect text for range {:?}",
                    start_ix..end_ix
                );
            }
        }

        let snapshot = list.read(cx).snapshot(cx);
        for (old_snapshot, subscription) in old_versions {
            let edits = subscription.consume().into_inner();

            log::info!(
                "applying edits since old text: {:?}: {:?}",
                old_snapshot.text(),
                edits,
            );

            let mut text = old_snapshot.text();
            for edit in edits {
                let new_text: String = snapshot.text_for_range(edit.new.clone()).collect();
                text.replace_range(edit.new.start..edit.new.start + edit.old.len(), &new_text);
            }
            assert_eq!(text.to_string(), snapshot.text());
        }
    }

    #[gpui::test(iterations = 100)]
    fn test_location(mut rng: StdRng) {
        let mut lhs = Default::default();
        let mut rhs = Default::default();
        while lhs == rhs {
            lhs = Location(
                (0..rng.gen_range(1..=5))
                    .map(|_| rng.gen_range(0..=100))
                    .collect(),
            );
            rhs = Location(
                (0..rng.gen_range(1..=5))
                    .map(|_| rng.gen_range(0..=100))
                    .collect(),
            );
        }

        if lhs > rhs {
            mem::swap(&mut lhs, &mut rhs);
        }

        let middle = Location::between(&lhs, &rhs);
        assert!(middle > lhs);
        assert!(middle < rhs);
        for ix in 0..middle.0.len() - 1 {
            assert!(
                middle.0[ix] == *lhs.0.get(ix).unwrap_or(&0)
                    || middle.0[ix] == *rhs.0.get(ix).unwrap_or(&0)
            );
        }
    }
}
