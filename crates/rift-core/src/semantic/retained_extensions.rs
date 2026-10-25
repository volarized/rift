use crate::Extensions;

pub(super) fn compact(extensions: &mut Extensions) -> usize {
    get_size2::GetSize::get_heap_size(extensions)
}
