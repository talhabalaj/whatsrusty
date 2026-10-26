"""Find the single WhatsRusty app in an export and validate its identity without changing it."""
from pathlib import Path
import plistlib
import sys


def canonical_app(directory):
    root = Path(directory)
    apps = list(root.glob('*.app'))
    if len(apps) != 1 or apps[0].is_symlink() or not apps[0].is_dir():
        raise ValueError('Expected exactly one top-level WhatsRusty app')
    app = apps[0]
    # Project, scheme and product share the name, so the Cloud artifact needs no renaming.
    if app.name != 'WhatsRusty.app':
        raise ValueError('Unexpected Cloud app export name')
    info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
    expected = dict(CFBundleIdentifier='me.balaj.WhatsRusty', CFBundleName='WhatsRusty',
                    CFBundleDisplayName='WhatsRusty', CFBundleExecutable='WhatsRusty')
    if any(info.get(key) != value for key, value in expected.items()):
        raise ValueError('Cloud app does not match the WhatsRusty product')
    return app


if __name__ == '__main__':
    print(canonical_app(sys.argv[1]))
