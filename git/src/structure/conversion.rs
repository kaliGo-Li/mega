use std::collections::HashMap;
use std::path::Path;
use std::{collections::HashSet, sync::Arc};

use crate::errors::GitError;
use crate::hash::Hash;
use crate::internal::object::commit::Commit;
use crate::internal::object::meta::Meta;
use crate::internal::object::tree::Tree;
use crate::internal::object::ObjectT;
use crate::internal::ObjectType;
use crate::protocol::{CommandType, PackProtocol, RefCommand};
// use crate::structure::nodes::build_node_tree;
use anyhow::Result;
use async_recursion::async_recursion;
use common::utils::ZERO_ID;
use database::driver::ObjectStorage;
use entity::{commit, node, refs};
use sea_orm::ActiveValue::NotSet;
use sea_orm::Set;

use super::nodes::Repo;

impl PackProtocol {
    pub async fn get_full_pack_data(&self, repo_path: &Path) -> Result<Vec<u8>, GitError> {
        let mut hash_meta: HashMap<String, Meta> = HashMap::new();

        let commit_models = self
            .storage
            .get_all_commits_by_path(repo_path)
            .await
            .unwrap();
        let mut commits = Vec::new();
        let mut tree_ids = Vec::new();

        for c_model in commit_models {
            let c = Commit::new_from_data(c_model.meta);
            tree_ids.push(c.tree_id.to_plain_str());
            commits.push(c);
        }
        let trees: HashMap<Hash, node::Model> = self
            .storage
            .get_nodes_by_ids(tree_ids)
            .await
            .unwrap()
            .into_iter()
            .map(|f| (Hash::new_from_str(&f.git_id), f))
            .collect();
        for commit in commits {
            hash_meta.insert(
                commit.id.to_plain_str(),
                Meta::new_from_data_with_object_type(ObjectType::Commit, commit.get_raw()),
            );
            if let Some(root) = trees.get(&commit.tree_id) {
                self.get_child_trees(root, &mut hash_meta).await
            } else {
                return Err(GitError::InvalidTreeObject(commit.tree_id.to_plain_str()));
            };
        }
        let result: Vec<u8> = vec![];
        // todo: add encode process
        // = encode(Some(hash_meta.into_values().collect()));
        Ok(result)
    }

    pub async fn get_incremental_pack_data(
        &self,
        repo_path: &Path,
        want: &HashSet<String>,
        _have: &HashSet<String>,
    ) -> Result<Vec<u8>, GitError> {
        let mut hash_meta: HashMap<String, Meta> = HashMap::new();
        let all_commits = self
            .storage
            .get_all_commits_by_path(repo_path)
            .await
            .unwrap();

        for c_data in all_commits {
            if want.contains(&c_data.git_id) {
                let c = Commit::new_from_data(c_data.meta);
                if let Some(root) = self.storage.get_node_by_id(&c.tree_id.to_plain_str()).await {
                    self.get_child_trees(&root, &mut hash_meta).await
                } else {
                    return Err(GitError::InvalidTreeObject(c.tree_id.to_plain_str()));
                };
            }
        }
        // todo: add encode process
        let result: Vec<u8> = vec![];
        // Pack::default().encode(Some(hash_meta.into_values().collect()));
        Ok(result)
    }

    // retrieve all sub trees recursively
    #[async_recursion]
    async fn get_child_trees(&self, root: &node::Model, hash_meta: &mut HashMap<String, Meta>) {
        let t = Tree::new_from_data(root.data.clone());
        let mut child_ids = vec![];
        for item in &t.tree_items {
            if !hash_meta.contains_key(&item.id.to_plain_str()) {
                child_ids.push(item.id.to_plain_str());
            }
        }
        let childs = self.storage.get_nodes_by_ids(child_ids).await.unwrap();
        for c in childs {
            if c.node_type == "tree" {
                self.get_child_trees(&c, hash_meta).await;
            } else {
                let b_meta = Meta::new_from_data_with_object_type(ObjectType::Blob, c.data);
                hash_meta.insert(b_meta.id.to_plain_str(), b_meta);
            }
        }
        let t_meta = Meta::new_from_data_with_object_type(ObjectType::Tree, t.get_raw());
        // tracing::info!("{}, {}", t_meta.id, t.tree_name);
        hash_meta.insert(t.id.to_plain_str(), t_meta);
    }

    pub async fn get_head_object_id(&self, repo_path: &Path) -> String {
        let path_str = repo_path.to_str().unwrap();
        let refs_list = self.storage.search_refs(path_str).await.unwrap();

        if refs_list.is_empty() {
            ZERO_ID.to_string()
        } else {
            for refs in &refs_list {
                if repo_path.to_str().unwrap() == refs.repo_path {
                    return refs.ref_git_id.clone();
                }
            }
            for refs in &refs_list {
                // if repo_path is subdirectory of some commit, we should generae a fake commit
                if repo_path.starts_with(refs.repo_path.clone()) {
                    return generate_child_commit_and_refs(self.storage.clone(), refs, repo_path)
                        .await;
                }
            }
            //situation: repo_path: root/repotest2/src, commit: root/repotest
            ZERO_ID.to_string()
        }
    }
}

/// Generates a new commit for a subdirectory of the original project directory.
/// Steps:
/// 1. Retrieve the root commit based on the provided reference's Git ID.
/// 2. If a root tree is found by searching for the repository path:
///    a. Construct a child commit using the retrieved root commit and the root tree.
///    b. Save the child commit.
///    c. Obtain the commit ID of the child commit.
///    d. Construct a child reference with the repository path, reference name, commit ID, and other relevant information.
///    e. Save the child reference in the database.
/// 3. Return the commit ID of the child commit if successful; otherwise, return a default ID.
pub async fn generate_child_commit_and_refs(
    storage: Arc<dyn ObjectStorage>,
    refs: &refs::Model,
    repo_path: &Path,
) -> String {
    if let Some(root_tree) = storage.search_root_node_by_path(repo_path).await {
        let root_commit = storage
            .get_commit_by_id(refs.ref_git_id.clone())
            .await
            .unwrap();
        let child_commit = Commit::build_from_model_and_root(&root_commit, root_tree);
        let child_model = child_commit.convert_to_model(repo_path);
        storage
            .save_commits(vec![child_model.clone()])
            .await
            .unwrap();
        let commit_id = child_commit.id.to_plain_str();
        let child_refs = refs::ActiveModel {
            id: NotSet,
            repo_path: Set(repo_path.to_str().unwrap().to_string()),
            ref_name: Set(refs.ref_name.clone()),
            ref_git_id: Set(commit_id.clone()),
            created_at: Set(chrono::Utc::now().naive_utc()),
            updated_at: Set(chrono::Utc::now().naive_utc()),
        };
        storage.save_refs(vec![child_refs]).await.unwrap();
        commit_id
    } else {
        ZERO_ID.to_string()
    }
}

pub async fn save_packfile(
    storage: Arc<dyn ObjectStorage>,
    mr_id: i64,
    repo_path: &Path,
) -> Result<(), anyhow::Error> {
    let mut repo = Repo {
        storage: storage.clone(),
        mr_id,
        tree_build_cache: HashSet::new(),
    };

    let mut commits: Vec<Commit> = Vec::new();
    let commit_model = storage.get_git_objects(mr_id, "commit").await.unwrap();
    for model in commit_model {
        commits.push(Commit::new_from_data(model.data))
    }

    let nodes = repo.build_node_tree(&commits).await.unwrap();
    storage.save_nodes(nodes).await.unwrap();

    let mut save_models: Vec<commit::ActiveModel> = Vec::new();
    for commit in commits {
        save_models.push(commit.convert_to_model(repo_path));
    }

    storage.save_commits(save_models).await.unwrap();
    Ok(())
}

pub async fn handle_refs(storage: Arc<dyn ObjectStorage>, command: &RefCommand, path: &Path) {
    match command.command_type {
        CommandType::Create => {
            storage
                .save_refs(vec![command.convert_to_model(path.to_str().unwrap())])
                .await
                .unwrap();
        }
        CommandType::Delete => storage.delete_refs(command.old_id.clone(), path).await,
        CommandType::Update => {
            storage
                .update_refs(command.old_id.clone(), command.new_id.clone(), path)
                .await;
        }
    }
}
