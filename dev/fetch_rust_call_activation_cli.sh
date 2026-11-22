#!/usr/bin/env bash
set -euo pipefail

role="$1"
root="$2"
api="repos/$GITHUB_REPOSITORY"
release=627e7355d0f7ec2821cf17dbb83b24fe8340163c
case "$role" in
  candidate)
    source_head=489071f7e0737962f2be39c7ec0927eabc3a5595
    checkout_head=10bff300d65c2a12020720e280cf52cdad20ef01
    tree=7d114fab44476910dc995df573dccf091d91c25e
    branch=perf/rust-call-cohort-v091
    event=pull_request
    run=45047922941
    job=137286755898
    artifact=14764826265
    bytes=148071861
    digest=33c465ee55a0d34ae095cca878c6cc52d8d7edbeafb3828ae2aa2b87ebebb623
    ;;
  control)
    source_head="$release"
    checkout_head="$release"
    tree=1065dae053c0b33efb26110a592d1a9bcbee8e8f
    branch=main
    event=push
    run=45029096687
    job=137228492130
    artifact=14757261924
    bytes=148020890
    digest=b00ec31d91b2cf6fed0b009d7522bcec405bb6b172899f459f3787bc9a3403b5
    ;;
  *)
    exit 2
    ;;
esac

out="$root/bin-$role"
mkdir "$out"
cd "$out"
case_dir="$root/reports"
gh api "$api/actions/runs/$run" --jq '{id,head_sha,head_branch,event,path,run_attempt}' > "$case_dir/$role-run.json"
jq -e --argjson run "$run" --arg head "$source_head" --arg branch "$branch" --arg event "$event" '
  .id==$run and .head_sha==$head and .head_branch==$branch and .event==$event
  and .path==".github/workflows/ci.yml" and .run_attempt==1
' "$case_dir/$role-run.json"
gh api "$api/actions/jobs/$job" --jq '{id,run_id,head_sha,head_branch,name,conclusion,labels}' > "$case_dir/$role-job.json"
jq -e --argjson run "$run" --argjson job "$job" --arg head "$source_head" --arg branch "$branch" '
  .id==$job and .run_id==$run and .head_sha==$head and .head_branch==$branch
  and .name=="integration-cli" and .conclusion=="success" and .labels==["ubuntu-24.04"]
' "$case_dir/$role-job.json"
gh api "$api/git/commits/$source_head" --jq '{sha,verified:.verification.verified}' > "$case_dir/$role-signature.json"
jq -e --arg head "$source_head" '.sha==$head and .verified' "$case_dir/$role-signature.json"
gh api "$api/git/commits/$checkout_head" --jq '{sha,tree:.tree.sha,parents:[.parents[].sha]}' > "$case_dir/$role-tree.json"
jq -e --arg commit "$checkout_head" --arg tree "$tree" --arg event "$event" --arg release "$release" --arg head "$source_head" '
  .sha==$commit and .tree==$tree and
  (if $event=="pull_request" then .parents==[$release,$head] else .sha==$release end)
' "$case_dir/$role-tree.json"
gh api "$api/actions/artifacts/$artifact" --jq '{id,name,size_in_bytes,digest,expired,workflow_run}' > "$case_dir/$role-artifact.json"
jq -e --argjson run "$run" --argjson artifact "$artifact" --arg head "$source_head"   --argjson bytes "$bytes" --arg digest "sha256:$digest" '
  .id==$artifact and .workflow_run.id==$run and .workflow_run.head_sha==$head and .name=="integration-cli"
  and (.expired|not) and .size_in_bytes==$bytes and .digest==$digest
' "$case_dir/$role-artifact.json"

gh api "$api/actions/artifacts/$artifact/zip" > artifact.zip
test "$(stat -c %s artifact.zip)" = "$bytes"
echo "$digest  artifact.zip" | sha256sum --check
test "$(unzip -Z1 artifact.zip)" = integration-cli.tar.zst
unzip -p artifact.zip integration-cli.tar.zst > integration-cli.tar.zst
test "$(tar --zstd -tf integration-cli.tar.zst)" = rift
test "$(tar --zstd -tvf integration-cli.tar.zst | cut -c1)" = -
tar --zstd -xOf integration-cli.tar.zst rift > rift
chmod +x rift
sha256sum artifact.zip integration-cli.tar.zst rift > "$case_dir/$role-sha256.txt"
./rift --version | tee "$case_dir/$role-version.txt"
test "$(cat "$case_dir/$role-version.txt")" = "rift 0.0.91"
