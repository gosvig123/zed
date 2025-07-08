// Every initial and invalidated state:
// [unresolved(0..len)]
//
// [pending(0..50), unresolved(51..len)]
//
// [hint(43), unresolved(51..len)]
// OR
// [unresolved(51..len)]

// [pending(0..50), unresolved(60..65), hint(70), hint(70), hint(90)]
//
// [pending(0..50), hint(70), hint(70), hint(90)]

use std::{ops::Range, sync::Arc};

use clock::Global;
use futures::future::Shared;
use gpui::Task;
use sum_tree::SumTree;
use text::{Anchor, Rope};

#[derive(Debug, Clone)]
enum LspInlayHintCacheItem {
    Unresolved {
        range: Range<Anchor>,
        attempts: usize,
    },
    Pending {
        range: Range<Anchor>,
        version: usize,
    },
    InlayHint {
        hint: InlayHint,
        version: usize,
    },
}

#[derive(Debug, Clone)]
struct Summary {
    // TODO kb
}

impl sum_tree::Summary for Summary {
    type Context = ();

    fn zero(cx: &Self::Context) -> Self {
        todo!()
    }

    fn add_summary(&mut self, summary: &Self, cx: &Self::Context) {
        todo!()
    }
}

impl sum_tree::Item for LspInlayHintCacheItem {
    type Summary = Summary;

    fn summary(&self, cx: &<Self::Summary as sum_tree::Summary>::Context) -> Self::Summary {
        todo!("TODO kb")
    }
}

#[derive(Debug, Clone, Copy)]
struct InlayHintId(usize);

#[derive(Debug, Clone)]
struct InlayHint {
    pub id: InlayHintId,
    pub position: Anchor,
    pub text: Rope,
}

// TODO kb wrong: we have to pull by ranges
type InlayHintsTask = Shared<Task<std::result::Result<Vec<InlayHint>, Arc<anyhow::Error>>>>;

#[derive(Debug)]
pub struct InlayHintCache {
    // TODO kb is it needed? What about the inlay hint data, should there be a version too?
    cache_version: usize,
    hints_update: Option<(Global, InlayHintsTask)>,
    items: SumTree<LspInlayHintCacheItem>,
}

impl InlayHintCache {
    /// Invalidate this cache. This will keep previously cached results until a
    /// call to `refresh` is made.
    pub fn invalidate(&mut self) {}

    /// Editor calls this every time when a viewport changes.
    pub fn refresh(&mut self, range: Range<usize>) {
        todo!()
    }

    /// Editor has to use this to keep its inlay may up-to-date,
    /// this is done once on editor instantiation for the initial inlay splice.
    ///
    /// The rest is retrieved via the updates.
    pub fn query(&self, range: Range<usize>) -> impl Iterator<Item = InlayHint> {
        let output: Vec<InlayHint> = todo!();
        output.into_iter()
    }
}

enum InlayHintsChanged {
    Added(Vec<InlayHint>),
    Removed(Vec<InlayHintId>),
}
