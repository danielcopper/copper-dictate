use crate::session::Insertion;

/// Where the desktop offers no way to insert: the final text reaches only `toggle`'s output.
pub(crate) struct NoInsertion;

impl Insertion for NoInsertion {
    fn insert(&self, _text: String) {}
}
