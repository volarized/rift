use crate::schema::{FieldFormat, FieldRole, FieldType};

#[derive(Clone, Debug)]
pub(crate) struct Registry {
    pub namespace: String,
    pub errors: Vec<Error>,
}

#[derive(Clone, Debug)]
pub(crate) struct Error {
    pub path: Vec<String>,
    pub slug: String,
    pub message: String,
    pub action: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    pub name: String,
    pub field_type: FieldType,
    pub optional: bool,
    pub format: Option<FieldFormat>,
    pub role: Option<FieldRole>,
    pub display: bool,
    pub sensitive: bool,
}
