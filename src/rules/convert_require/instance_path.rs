use crate::nodes::{FieldExpression, FunctionCall, Identifier, Prefix, StringExpression};

use super::RobloxIndexStyle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstancePath {
    root: InstancePathRoot,
    components: Vec<InstancePathComponent>,
}

impl InstancePath {
    pub(crate) fn from_root() -> Self {
        Self {
            root: InstancePathRoot::Root,
            components: Vec::new(),
        }
    }

    pub(crate) fn from_script() -> Self {
        Self {
            root: InstancePathRoot::Script,
            components: Vec::new(),
        }
    }

    pub(crate) fn parent(&mut self) {
        self.components.push(InstancePathComponent::Parent);
    }

    pub(crate) fn child(&mut self, child_name: impl Into<String>) {
        self.components
            .push(InstancePathComponent::Child(child_name.into()));
    }

    // NEW: represent an ancestor lookup by name
    pub(crate) fn ancestor(&mut self, ancestor_name: impl Into<String>) {
        self.components
            .push(InstancePathComponent::Ancestor(ancestor_name.into()));
    }

    pub(crate) fn convert(&self, index_style: &RobloxIndexStyle) -> Prefix {
        let mut components_iter = self.components.iter();

        let mut prefix = match &self.root {
            InstancePathRoot::Root => {
                let mut prefix: Prefix = datamodel_identifier().into();
                if let Some(InstancePathComponent::Child(service_name)) = components_iter.next() {
                    prefix = FunctionCall::from_prefix(prefix)
                        .with_method("GetService")
                        .with_argument(StringExpression::from_value(service_name))
                        .into();
                }
                prefix
            }
            InstancePathRoot::Script => script_identifier().into(),
        };

        for component in components_iter {
            match component {
                InstancePathComponent::Parent => {
                    prefix = get_parent_instance(prefix);
                }
                InstancePathComponent::Child(child_name) => {
                    prefix = index_style.index(prefix, child_name);
                }
                InstancePathComponent::Ancestor(ancestor_name) => {
                    prefix = FunctionCall::from_prefix(prefix)
                        .with_method("FindFirstAncestor")
                        .with_argument(StringExpression::from_value(ancestor_name))
                        .into();
                }
            }
        }

        prefix
    }

    // NEW: accessors used by the sourcemap to traverse the instance path
    pub(crate) fn root(&self) -> &InstancePathRoot {
        &self.root
    }

    pub(crate) fn components(&self) -> &[InstancePathComponent] {
        &self.components
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstancePathRoot {
    Root,
    Script,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstancePathComponent {
    Parent,
    Child(String),
    Ancestor(String),
}

pub(crate) fn script_identifier() -> Identifier {
    Identifier::new("script")
}

pub(crate) fn datamodel_identifier() -> Identifier {
    Identifier::new("game")
}

pub(crate) fn get_parent_instance(instance: impl Into<Prefix>) -> Prefix {
    FieldExpression::new(instance.into(), "Parent").into()
}
