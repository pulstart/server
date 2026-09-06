"""Packaging regression tests; mirrored in server/scripts."""
import importlib.util
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("bundle", Path(__file__).with_name("bundle-macos-libs.py"))
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)


class BundleTests(unittest.TestCase):
    def test_load_command_parser_preserves_spaces_and_reexports(self):
        dependencies, rpaths = bundle.parse_load_commands('''
          cmd LC_ID_DYLIB
         name @rpath/self.dylib (offset 24)
          cmd LC_LOAD_DYLIB
         name /some directory/libfoo.dylib (offset 24)
          cmd LC_REEXPORT_DYLIB
         name @rpath/libbar.dylib (offset 24)
          cmd LC_LOAD_WEAK_DYLIB
         name /usr/lib/liboptional.dylib (offset 24)
          cmd LC_RPATH
         path @loader_path/../lib (offset 12)
        ''')
        self.assertEqual(dependencies, ["/some directory/libfoo.dylib", "@rpath/libbar.dylib", "/usr/lib/liboptional.dylib"])
        self.assertEqual(rpaths, ["@loader_path/../lib"])

    def test_system_swift_can_resolve_from_dyld_cache(self):
        result = bundle.resolve_dependency("@rpath/libswift_Concurrency.dylib", Path("/app/main"), Path("/app/main"), [Path("/usr/lib/swift")])
        self.assertEqual(result, Path("/usr/lib/swift/libswift_Concurrency.dylib"))

    def test_missing_library_is_a_packaging_error(self):
        with self.assertRaisesRegex(ValueError, "Unresolved dependency"):
            bundle.resolve_dependency("@rpath/libmissing.dylib", Path("/app/main"), Path("/app/main"), [])

    def test_swift_rpath_does_not_hide_missing_third_party_dependency(self):
        with self.assertRaisesRegex(ValueError, "Unresolved dependency"):
            bundle.resolve_dependency("@rpath/libmissing.dylib", Path("/app/main"), Path("/app/main"), [Path("/usr/lib/swift")])

    def test_library_name_collision_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "source"
            source.touch()
            app = root / "App.app/Contents/MacOS/main"
            app.parent.mkdir(parents=True)
            app.touch()
            libs = [root / sub / "libsame.dylib" for sub in ["one", "two"]]
            for lib in libs:
                lib.parent.mkdir()
                lib.touch()
            with patch.object(bundle, "inspect", return_value=([str(p) for p in libs], [])):
                with self.assertRaisesRegex(ValueError, "Conflicting bundled library"):
                    bundle.bundle_libraries(source, app)

    def test_audit_rejects_build_machine_library(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            library = root / "external.dylib"
            library.touch()
            app = root / "App.app/Contents/MacOS/main"
            app.parent.mkdir(parents=True)
            app.touch()
            with patch.object(bundle, "inspect", return_value=([str(library)], [])):
                with self.assertRaisesRegex(ValueError, "external library"):
                    bundle.audit_bundle(app)

    @unittest.skipUnless(sys.platform == "darwin", "requires native Mach-O tools and dyld")
    def test_relocated_app_runs_without_original_transitive_libraries(self):
        with tempfile.TemporaryDirectory(prefix="st bundle test ") as tmp:
            root = Path(tmp)
            external = root / "build libs"
            external.mkdir()
            leaf = external / "libleaf.1.dylib"
            middle = external / "libmiddle.dylib"
            main = root / "main"
            (root / "leaf.c").write_text("int leaf(void) { return 42; }")
            (root / "middle.c").write_text("extern int leaf(void); int middle(void) { return leaf(); }")
            (root / "main.c").write_text("extern int middle(void); int main(void) { return middle() == 42 ? 0 : 1; }")
            subprocess.run(["cc", "-dynamiclib", str(root / "leaf.c"), "-o", str(leaf), "-Wl,-install_name,@rpath/libleaf.1.dylib"], check=True)
            (external / "libleaf.dylib").symlink_to(leaf.name)
            subprocess.run(["cc", "-dynamiclib", str(root / "middle.c"), "-o", str(middle), "-L" + str(external), "-lleaf", "-Wl,-rpath,@loader_path", "-Wl,-install_name," + str(middle)], check=True)
            subprocess.run(["cc", str(root / "main.c"), "-o", str(main), "-L" + str(external), "-lmiddle", "-Wl,-rpath," + str(external)], check=True)
            app = root / "App.app/Contents/MacOS/main"
            app.parent.mkdir(parents=True)
            shutil.copy2(main, app)
            libraries = bundle.bundle_libraries(main, app)
            self.assertEqual(len(libraries), 2)
            for executable in [*libraries, app]:
                subprocess.run(["codesign", "--force", "--sign", "-", str(executable)], check=True)
            external.rename(root / "unavailable")
            bundle.audit_bundle(app)
            subprocess.run([str(app)], check=True)


if __name__ == "__main__":
    unittest.main()
