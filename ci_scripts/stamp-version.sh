#!/bin/sh
# Stamp CFBundleVersion from the Xcode Cloud build number and the marketing version from the v tag.
set -eu
: "${CI_PRIMARY_REPOSITORY_PATH:?}"
: "${CI_BUILD_NUMBER:?}"
cd "$CI_PRIMARY_REPOSITORY_PATH"
python3 - <<'PYC'
from pathlib import Path
import os, re
p = Path('WhatsRusty.xcodeproj/project.pbxproj')
build = os.environ['CI_BUILD_NUMBER']; tag = os.environ.get('CI_TAG', '')
if not re.fullmatch(r'[1-9][0-9]*', build): raise SystemExit('Invalid cloud build number')
s = p.read_text(); s = re.sub(r'CURRENT_PROJECT_VERSION = [^;]+;', f'CURRENT_PROJECT_VERSION = {build};', s)
if tag:
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', tag): raise SystemExit('Use a numeric version tag')
    s = re.sub(r'MARKETING_VERSION = [^;]+;', f'MARKETING_VERSION = {tag[1:]};', s)
p.write_text(s)
PYC
