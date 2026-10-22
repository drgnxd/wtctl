use super::*;

// Older releases may have kept task tips here; these refs still count as preserved.
pub fn kept_tips(repo: &Repo, id: &str) -> Vec<String> {
    git::run(
        &repo.root,
        [
            "for-each-ref",
            "--format=%(objectname)",
            &format!("refs/wtctl/superseded/{id}.*"),
        ],
    )
    .map(|text| text.lines().map(str::to_string).collect())
    .unwrap_or_default()
}
