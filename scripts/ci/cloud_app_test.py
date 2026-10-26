from pathlib import Path
import plistlib
import runpy
import tempfile
import unittest
canonical_app = runpy.run_path(str(Path(__file__).with_name('cloud-app.py')))['canonical_app']

class CloudAppTests(unittest.TestCase):
    def make(self, root, name='WhatsRusty.app', **overrides):
        app = Path(root) / name
        (app / 'Contents').mkdir(parents=True)
        info = dict(CFBundleIdentifier='me.balaj.WhatsRusty', CFBundleName='WhatsRusty',
                    CFBundleDisplayName='WhatsRusty', CFBundleExecutable='WhatsRusty')
        info.update(overrides)
        (app / 'Contents/Info.plist').write_bytes(plistlib.dumps(info))
        return app

    def test_product_named_export_preserves_path_and_contents(self):
        with tempfile.TemporaryDirectory() as root:
            app = self.make(root)
            before = (app / 'Contents/Info.plist').read_bytes()
            (app / 'Contents/Frameworks').mkdir()
            result = canonical_app(root)
            self.assertEqual(result, app)
            self.assertEqual((result / 'Contents/Info.plist').read_bytes(), before)

    def test_debug_product_is_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            self.make(root, 'WhatsRusty Dev.app', CFBundleIdentifier='me.balaj.WhatsRusty.dev',
                      CFBundleName='WhatsRusty Dev', CFBundleDisplayName='WhatsRusty Dev', CFBundleExecutable='WhatsRusty Dev')
            with self.assertRaises(ValueError): canonical_app(root)

    def test_wrong_product_and_ambiguous_exports_are_rejected(self):
        for mode in ['name', 'identity', 'executable', 'legacy', 'multiple', 'symlink']:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as root:
                name = {'name': 'Other.app', 'legacy': 'WhatsApp GPUI.app'}.get(mode, 'WhatsRusty.app')
                overrides = {'identity': {'CFBundleIdentifier': 'com.talhabalaj.whatsapp-gpui'},
                             'executable': {'CFBundleExecutable': 'whatsapp-gpui'}}.get(mode, {})
                app = self.make(root, name, **overrides)
                if mode == 'multiple': self.make(root, 'Other.app')
                if mode == 'symlink':
                    app.rename(Path(root) / 'original')
                    app.symlink_to('original', target_is_directory=True)
                with self.assertRaises(ValueError): canonical_app(root)

if __name__ == '__main__':
    unittest.main()
