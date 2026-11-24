//! Objects and references in an Xcode project.

use std::collections::BTreeMap;

use rcmake_gen_common::GenError;

use crate::ids;

macro_rules! object_types {
    ($($name:ident = $tag:literal),* $(,)?) => {
        #[derive(Clone, Copy, Debug)]
        pub(crate) enum Isa { $($name = $tag),* }

        impl Isa {
            pub(crate) fn name(self) -> &'static str {
                match self { $(Self::$name => stringify!($name)),* }
            }
        }
    };
}

object_types! {
    PBXGroup = 0,
    PBXProject = 1,
    PBXHeadersBuildPhase = 2,
    PBXSourcesBuildPhase = 3,
    PBXFrameworksBuildPhase = 4,
    PBXNativeTarget = 5,
    PBXFileReference = 6,
    PBXBuildFile = 7,
    PBXContainerItemProxy = 8,
    PBXTargetDependency = 9,
    PBXShellScriptBuildPhase = 10,
    PBXResourcesBuildPhase = 11,
    PBXAggregateTarget = 17,
    XCBuildConfiguration = 18,
    XCConfigurationList = 19,
    PBXCopyFilesBuildPhase = 20,
}

#[derive(Clone, Debug)]
pub(crate) struct Reference {
    pub(crate) id: String,
    pub(crate) comment: String,
}

#[derive(Clone, Debug)]
pub(crate) enum Value {
    Text(String),
    QuotedText(String),
    Ref(Reference),
    List(Vec<Value>),
    ListRow(Vec<String>),
    Dict(Attrs),
}

pub(crate) type Attrs = BTreeMap<String, Value>;

impl<T: Into<String>> From<T> for Value {
    fn from(text: T) -> Self {
        Self::Text(text.into())
    }
}

impl From<Reference> for Value {
    fn from(reference: Reference) -> Self {
        Self::Ref(reference)
    }
}

pub(crate) fn list<T: Into<Value>>(items: impl IntoIterator<Item = T>) -> Value {
    Value::List(items.into_iter().map(Into::into).collect())
}

pub(crate) fn attrs<const N: usize>(items: [(&str, Value); N]) -> Attrs {
    items
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
}

pub(crate) struct Object {
    pub(crate) isa: Isa,
    pub(crate) reference: Reference,
    pub(crate) attrs: Attrs,
}

#[derive(Default)]
pub(crate) struct Objects {
    keys: BTreeMap<String, String>,
    pub(crate) entries: BTreeMap<(&'static str, String), Object>,
}

impl Objects {
    pub(crate) fn reserve(
        &mut self,
        isa: Isa,
        key: &str,
        comment: &str,
        cached: Option<&str>,
    ) -> Result<Reference, GenError> {
        let id = cached.map_or_else(|| ids::generate(isa, key), str::to_owned);
        if let Some(previous) = self.keys.get(&id) {
            return Err(GenError::Invalid(
                format!("Xcode object ID {id} is shared by {previous} and {key}").into(),
            ));
        }
        self.keys.insert(id.clone(), key.into());
        Ok(Reference {
            id,
            comment: comment.into(),
        })
    }

    pub(crate) fn insert(&mut self, isa: Isa, reference: &Reference, attrs: Attrs) {
        self.entries.insert(
            (isa.name(), reference.id.clone()),
            Object {
                isa,
                reference: reference.clone(),
                attrs,
            },
        );
    }

    pub(crate) fn add(
        &mut self,
        isa: Isa,
        key: &str,
        comment: &str,
        attrs: Attrs,
    ) -> Result<Reference, GenError> {
        let reference = self.reserve(isa, key, comment, None)?;
        self.insert(isa, &reference, attrs);
        Ok(reference)
    }
}
