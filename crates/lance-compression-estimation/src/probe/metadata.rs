use lance_core::datatypes::{BLOB_V2_DESC_LANCE_FIELD, Schema};

/// Older writers left Blob v2 descriptor children with unassigned IDs, so the
/// footer reader exposes them as top-level fields. Restore their parent before
/// counting physical columns; the whole descriptor occupies one column.
pub(super) fn restore_blob_children(schema: &Schema) -> Schema {
    let mut schema = schema.clone();
    let expected = &BLOB_V2_DESC_LANCE_FIELD.children;
    let mut index = 0;
    while index < schema.fields.len() {
        let field = &schema.fields[index];
        let end = index + 1 + expected.len();
        if field.is_blob()
            && field.logical_type == BLOB_V2_DESC_LANCE_FIELD.logical_type
            && field.children.is_empty()
            && end <= schema.fields.len()
            && schema.fields[index + 1..end]
                .iter()
                .zip(expected)
                .all(|(child, expected)| {
                    child.id == -1
                        && child.parent_id == -1
                        && child.name == expected.name
                        && child.data_type() == expected.data_type()
                })
        {
            let children = schema.fields.drain(index + 1..end).collect();
            schema.fields[index].children = children;
        }
        index += 1;
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_blob_children_do_not_shift_the_following_column() {
        let mut blob = BLOB_V2_DESC_LANCE_FIELD.clone();
        blob.id = 0;
        let children = std::mem::take(&mut blob.children);
        let next = lance_core::datatypes::Field::try_from(&arrow_schema::Field::new(
            "next",
            arrow_schema::DataType::Int32,
            true,
        ))
        .unwrap();
        let mut schema = Schema::default();
        schema.fields.push(blob);
        schema.fields.extend(children);
        schema.fields.push(next);
        let restored = restore_blob_children(&schema);
        assert_eq!(restored.fields.len(), 2);
        assert_eq!(
            restored.fields[0].children.len(),
            BLOB_V2_DESC_LANCE_FIELD.children.len()
        );
        assert_eq!(restored.fields[1].name, "next");
        assert_eq!(
            lance_file::versions::physical_column_count(
                lance_file::version::ConcreteFileVersion::V2_2,
                &restored.fields[0]
            ),
            1
        );
    }
}
