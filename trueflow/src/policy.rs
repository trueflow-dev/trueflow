use crate::block::Block;
use crate::config::BlockFilters;

pub fn should_skip_imports_by_default(path: &str, block: &Block, filters: &BlockFilters) -> bool {
    if block.kind.is_import_like() && !is_lib_rs(path) && !filters.only_contains(&block.kind) {
        return true;
    }
    false
}

fn is_lib_rs(path: &str) -> bool {
    path.ends_with("/lib.rs") || path == "lib.rs"
}
