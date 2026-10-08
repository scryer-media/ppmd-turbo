package fixtures

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func write(t *testing.T, path, text string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(text), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestLoadChecksSchemaAndFiles(t *testing.T) {
	dir := t.TempDir()
	manifest := `{"schema":"` + Schema + `","profile":"quick","archives":[{"name":"a","file":"a.7z"}],"raw":[],"sources":[]}`
	write(t, filepath.Join(dir, ManifestName), manifest)
	if _, err := Load(dir); err == nil || !strings.Contains(err.Error(), "a.7z") {
		t.Fatalf("missing file not reported: %v", err)
	}
	write(t, filepath.Join(dir, "a.7z"), "x")
	loaded, err := Load(dir)
	if err != nil || loaded.Profile != "quick" || len(loaded.Archives) != 1 {
		t.Fatalf("got %+v, %v", loaded, err)
	}
	write(t, filepath.Join(dir, ManifestName), `{"schema":"something-else"}`)
	if _, err := Load(dir); err == nil {
		t.Fatal("wrong schema accepted")
	}
}

func TestFindRAR(t *testing.T) {
	if _, err := FindRAR("", true); !errors.Is(err, ErrNoRARCorpus) {
		t.Fatalf("empty dir: %v", err)
	}
	dir := t.TempDir()
	write(t, filepath.Join(dir, "rar4_ppm_solid_restart.rar"), "rar")
	write(t, filepath.Join(dir, "rar4_ppm_order16_32m.rar"), "rar!")
	small, err := FindRAR(dir, false)
	if err != nil || len(small) != 1 || small[0].Bytes != 3 {
		t.Fatalf("small: %+v %v", small, err)
	}
	all, err := FindRAR(dir, true)
	if err != nil || len(all) != 2 || all[1].Unpacked != 32<<20 {
		t.Fatalf("all: %+v %v", all, err)
	}
}
