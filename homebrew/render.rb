#!/usr/bin/env ruby
# frozen_string_literal: true

# Render homebrew/localdb.rb.erb from a dist dist-manifest.json plus the
# downloaded release artifacts (for the sibling `<tarball>.sha256` files —
# dist's manifest names the checksum artifact but does not inline the hex).
#
#   ruby homebrew/render.rb <dist-manifest.json> <artifacts-dir> <template.erb> > localdb.rb
#
# URLs and sha256s come verbatim from dist's outputs; this script only
# selects the per-target tarball artifacts.

require "erb"
require "json"

abort "usage: render.rb <dist-manifest.json> <artifacts-dir> <template.erb>" unless ARGV.length == 3

manifest = JSON.parse(File.read(ARGV[0]))
artifacts_dir = ARGV[1]
template = File.read(ARGV[2])

release = manifest.fetch("releases").find { |r| r.fetch("app_name") == "localdb" }
abort "no localdb release in manifest" unless release

version = release.fetch("app_version")

# Download URLs come from the manifest's hosting info, which dist builds from
# the tag the release run was actually triggered by — never reconstructed
# from the version (the workflow's tag pattern accepts more shapes than
# `vX.Y.Z`, and a fabricated tag would 404).
hosting = release.dig("hosting", "github")
abort "manifest has no github hosting info (not produced by `dist host`?)" unless hosting
base = hosting.fetch("artifact_base_url").chomp("/") + hosting.fetch("artifact_download_path")
base += "/" unless base.end_with?("/")

url = {}
sha256 = {}
manifest.fetch("artifacts").each do |name, artifact|
  next unless artifact["kind"] == "executable-zip"
  next unless name.end_with?(".tar.xz", ".tar.gz")

  checksum_file = File.join(artifacts_dir, "#{name}.sha256")
  abort "missing checksum file #{checksum_file}" unless File.exist?(checksum_file)
  # Format: "<hex> *<filename>"
  checksum = File.read(checksum_file).split.first
  abort "malformed checksum in #{checksum_file}" unless checksum&.match?(/\A[0-9a-f]{64}\z/)

  (artifact["target_triples"] || []).each do |triple|
    url[triple] = base + name
    sha256[triple] = checksum
  end
end

%w[aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu].each do |t|
  abort "manifest is missing artifact for #{t}" unless url[t] && sha256[t]
end

puts ERB.new(template, trim_mode: "-").result_with_hash(
  version: version, url: url, sha256: sha256
)
