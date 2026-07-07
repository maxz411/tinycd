# Publishing a release

One-time setup (already done or nearly done):

- `maxz411/homebrew-tap` repository with this directory's `homebrew/tinycd.rb`
  at `Formula/tinycd.rb`. Users install with `brew install maxz411/tap/tinycd`.
- `maxz411/scoop-bucket` repository with `scoop/tinycd.json` at
  `bucket/tinycd.json`. Users install with
  `scoop bucket add maxz411 https://github.com/maxz411/scoop-bucket`
  then `scoop install tinycd`.
- A crates.io account with a verified email and `cargo login` run once.

Per release:

1. Bump `version` in `Cargo.toml`, run `cargo build` so `Cargo.lock` follows,
   commit, and push.
2. `git tag vX.Y.Z && git push origin vX.Y.Z`. The release workflow builds
   the binaries and publishes the GitHub release with `.sha256` files.
3. `cargo publish`.
4. Update the tap: copy `homebrew/tinycd.rb` with the new version in the URLs
   and the `sha256` values from the release's `.sha256` assets:

   ```sh
   for t in aarch64-apple-darwin x86_64-apple-darwin \
            aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
     curl -fsSL "https://github.com/maxz411/tinycd/releases/download/vX.Y.Z/tinycd-$t.tar.gz.sha256"
   done
   ```

5. Update the bucket's `bucket/tinycd.json` the same way (or leave it to
   Scoop's autoupdate, which reads the `.sha256` asset itself).

The install scripts (`install.sh`, `install.ps1`) always fetch the latest
release and need no per-release changes.
