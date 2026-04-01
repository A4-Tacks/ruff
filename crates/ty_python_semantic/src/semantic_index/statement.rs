use crate::ast_node_ref::AstNodeRef;
use crate::db::Db;
use crate::node_key::NodeKey;
use crate::semantic_index::scope::{FileScopeId, ScopeId};
use ruff_db::files::File;
use ruff_python_ast as ast;
use salsa;

/// An independently type-inferable statement.
///
/// ## Module-local type
/// This type should not be used as part of any cross-module API because
/// it holds a reference to the AST node. Range-offset changes
/// then propagate through all usages, and deserialization requires
/// reparsing the entire module.
///
/// E.g. don't use this type in:
///
/// * a return type of a cross-module query
/// * a field of a type that is a return type of a cross-module query
/// * an argument of a cross-module query
#[salsa::tracked(debug, heap_size=ruff_memory_usage::heap_size)]
pub(crate) struct Statement<'db> {
    /// The file in which the statement occurs.
    pub(crate) file: File,

    /// The scope in which the statement occurs.
    pub(crate) file_scope: FileScopeId,

    /// The statement node.
    #[no_eq]
    #[tracked]
    #[returns(ref)]
    pub(crate) node_ref: AstNodeRef<ast::Stmt>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for Statement<'_> {}

impl<'db> Statement<'db> {
    pub(crate) fn scope(self, db: &'db dyn Db) -> ScopeId<'db> {
        self.file_scope(db).to_scope_id(db, self.file(db))
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, salsa::Update, get_size2::GetSize)]
pub(crate) struct StatementNodeKey(NodeKey);

impl From<&ast::Stmt> for StatementNodeKey {
    fn from(node: &ast::Stmt) -> Self {
        Self(NodeKey::from_node(node))
    }
}
