// Helpers the recipes share. Loaded into every session with the recipes.

/// The name of a status, whether the variant carries data or not.
///
/// A fieldless variant is a string; a variant with data is a one-key map
/// under its name; an internally tagged enum (the L1 broadcast status) is a
/// map with the name under `status`.
fn status_name(status) {
    if status.type_of() != "map" { return status; }
    if "status" in status { status.status } else { status.keys()[0] }
}

/// Counts the rows of `table` by the name of `field`'s status variant.
fn count_by_status(table, field) {
    let counts = #{};
    let n = count_where(table, |r| {
        let name = status_name(r.value[field]);
        counts[name] = if name in counts { counts[name] + 1 } else { 1 };
        false
    });
    counts
}

/// Stages a delete if the key is present; returns how many were staged.
fn del_if_present(table, key) {
    if get(table, key) == () { 0 } else { del(table, key); 1 }
}
