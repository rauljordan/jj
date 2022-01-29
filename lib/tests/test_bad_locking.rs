// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::path::Path;

use jujutsu_lib::repo::ReadonlyRepo;
use jujutsu_lib::testutils;
use jujutsu_lib::workspace::Workspace;
use tempfile::TempDir;
use test_case::test_case;

fn copy_directory(src: &Path, dst: &Path) {
    std::fs::create_dir(dst).ok();
    for entry in std::fs::read_dir(src).unwrap() {
        let child_src = entry.unwrap().path();
        let base_name = child_src.file_name().unwrap();
        let child_dst = dst.join(base_name);
        if child_src.is_dir() {
            copy_directory(&child_src, &child_dst)
        } else {
            std::fs::copy(&child_src, &child_dst).unwrap();
        }
    }
}

fn merge_directories(left: &Path, base: &Path, right: &Path, output: &Path) {
    std::fs::create_dir(output).ok();
    let mut sub_dirs = vec![];
    // Walk the left side and copy to the output
    for entry in std::fs::read_dir(left).unwrap() {
        let path = entry.unwrap().path();
        let base_name = path.file_name().unwrap();
        let child_left = left.join(base_name);
        let child_output = output.join(base_name);
        if child_left.is_dir() {
            sub_dirs.push(base_name.to_os_string());
        } else {
            std::fs::copy(&child_left, &child_output).unwrap();
        }
    }
    // Walk the base and find files removed in the right side, then remove them in
    // the output
    for entry in std::fs::read_dir(base).unwrap() {
        let path = entry.unwrap().path();
        let base_name = path.file_name().unwrap();
        let child_base = base.join(base_name);
        let child_right = right.join(base_name);
        let child_output = output.join(base_name);
        if child_base.is_dir() {
            sub_dirs.push(base_name.to_os_string());
        } else if !child_right.exists() {
            std::fs::remove_file(child_output).ok();
        }
    }
    // Walk the right side and find files added in the right side, then add them in
    // the output
    for entry in std::fs::read_dir(right).unwrap() {
        let path = entry.unwrap().path();
        let base_name = path.file_name().unwrap();
        let child_base = base.join(base_name);
        let child_right = right.join(base_name);
        let child_output = output.join(base_name);
        if child_right.is_dir() {
            sub_dirs.push(base_name.to_os_string());
        } else if !child_base.exists() {
            // This overwrites the left side if that's been written. That's fine, since the
            // point of the test is that it should be okay for either side to win.
            std::fs::copy(&child_right, &child_output).unwrap();
        }
    }
    // Do the merge in subdirectories
    for base_name in sub_dirs {
        let child_base = base.join(&base_name);
        let child_right = right.join(&base_name);
        let child_left = left.join(&base_name);
        let child_output = output.join(&base_name);
        merge_directories(&child_left, &child_base, &child_right, &child_output);
    }
}

#[test_case(false ; "local backend")]
#[test_case(true ; "git backend")]
fn test_bad_locking_children(use_git: bool) {
    // Test that two new commits created on separate machines are both visible (not
    // lost due to lack of locking)
    let settings = testutils::user_settings();
    let test_workspace = testutils::init_repo(&settings, use_git);
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root();

    let mut tx = repo.start_transaction("test");
    let initial = testutils::create_random_commit(&settings, repo)
        .set_parents(vec![repo.store().root_commit_id().clone()])
        .write_to_repo(tx.mut_repo());
    tx.commit();

    // Simulate a write of a commit that happens on one machine
    let machine1_root = TempDir::new().unwrap().into_path();
    copy_directory(workspace_root, &machine1_root);
    let machine1_workspace = Workspace::load(&settings, machine1_root.clone()).unwrap();
    let machine1_repo = machine1_workspace.repo_loader().load_at_head();
    let mut machine1_tx = machine1_repo.start_transaction("test");
    let child1 = testutils::create_random_commit(&settings, &machine1_repo)
        .set_parents(vec![initial.id().clone()])
        .write_to_repo(machine1_tx.mut_repo());
    machine1_tx.commit();

    // Simulate a write of a commit that happens on another machine
    let machine2_root = TempDir::new().unwrap().into_path();
    copy_directory(workspace_root, &machine2_root);
    let machine2_workspace = Workspace::load(&settings, machine2_root.clone()).unwrap();
    let machine2_repo = machine2_workspace.repo_loader().load_at_head();
    let mut machine2_tx = machine2_repo.start_transaction("test");
    let child2 = testutils::create_random_commit(&settings, &machine2_repo)
        .set_parents(vec![initial.id().clone()])
        .write_to_repo(machine2_tx.mut_repo());
    machine2_tx.commit();

    // Simulate that the distributed file system now has received the changes from
    // both machines
    let merged_path = TempDir::new().unwrap().into_path();
    merge_directories(&machine1_root, workspace_root, &machine2_root, &merged_path);
    let merged_workspace = Workspace::load(&settings, merged_path).unwrap();
    let merged_repo = merged_workspace.repo_loader().load_at_head();
    assert!(merged_repo.view().heads().contains(child1.id()));
    assert!(merged_repo.view().heads().contains(child2.id()));
    let op_id = merged_repo.op_id().clone();
    let op = merged_repo.op_store().read_operation(&op_id).unwrap();
    assert_eq!(op.parents.len(), 2);
}

#[test_case(false ; "local backend")]
#[test_case(true ; "git backend")]
fn test_bad_locking_interrupted(use_git: bool) {
    // Test that an interrupted update of the op-heads resulting in on op-head
    // that's a descendant of the other is resolved without creating a new
    // operation.
    let settings = testutils::user_settings();
    let test_workspace = testutils::init_repo(&settings, use_git);
    let repo = &test_workspace.repo;

    let mut tx = repo.start_transaction("test");
    let initial = testutils::create_random_commit(&settings, repo)
        .set_parents(vec![repo.store().root_commit_id().clone()])
        .write_to_repo(tx.mut_repo());
    let repo = tx.commit();

    // Simulate a crash that resulted in the old op-head left in place. We simulate
    // it somewhat hackily by copying the .jj/op_heads/ directory before the
    // operation and then copying that back afterwards, leaving the existing
    // op-head(s) in place.
    let op_heads_dir = repo.repo_path().join("op_heads");
    let backup_path = TempDir::new().unwrap().into_path();
    copy_directory(&op_heads_dir, &backup_path);
    let mut tx = repo.start_transaction("test");
    testutils::create_random_commit(&settings, &repo)
        .set_parents(vec![initial.id().clone()])
        .write_to_repo(tx.mut_repo());
    let op_id = tx.commit().operation().id().clone();

    copy_directory(&backup_path, &op_heads_dir);
    // Reload the repo and check that only the new head is present.
    let reloaded_repo = ReadonlyRepo::load(&settings, repo.repo_path().clone());
    assert_eq!(reloaded_repo.op_id(), &op_id);
    // Reload once more to make sure that the .jj/op_heads/ directory was updated
    // correctly.
    let reloaded_repo = ReadonlyRepo::load(&settings, repo.repo_path().clone());
    assert_eq!(reloaded_repo.op_id(), &op_id);
}
