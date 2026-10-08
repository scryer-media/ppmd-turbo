package toolchain

import "testing"

func TestParseBanner(t *testing.T) {
	cases := []struct{ text, marker, version string }{
		{"\n7-Zip (z) 26.01 (arm64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-04-27\n 64-bit", "7-Zip", "26.01"},
		{"7-Zip [64] 16.02 : Copyright (c) 1999-2016 Igor Pavlov : 2016-05-21\np7zip Version 16.02", "7-Zip", "16.02"},
		{"\nUNRAR 7.11 freeware      Copyright (c) 1993-2025 Alexander Roshal\n\nUsage:", "UNRAR", "7.11"},
	}
	for _, c := range cases {
		banner, version := ParseBanner(c.text, c.marker)
		if banner == "" || version != c.version {
			t.Errorf("ParseBanner(%q) = %q, %q; want version %q", c.text, banner, version, c.version)
		}
	}
	if banner, _ := ParseBanner("usage: something else", "7-Zip"); banner != "" {
		t.Errorf("unrelated text gave banner %q", banner)
	}
}

func TestHasRARCodec(t *testing.T) {
	without := "Formats:\n ... Rar  rar r00\n\nCodecs:\n    ED     30401 PPMD\n\nHashers:\n"
	with := "Codecs:\n    ED     30401 PPMD\n    D      40303 Rar3\n\nHashers:\n      4        1 CRC32\n"
	if HasRARCodec(without) || !HasRARCodec(with) {
		t.Fatalf("without %t with %t", HasRARCodec(without), HasRARCodec(with))
	}
}

func TestLockedVersion(t *testing.T) {
	lock := "[[package]]\nname = \"crc32fast\"\nversion = \"1.5.0\"\n\n[[package]]\nname = \"ppmd-rust\"\nversion = \"1.5.0\"\nsource = \"registry\"\n"
	if got := LockedVersion(lock, "ppmd-rust"); got != "1.5.0" {
		t.Fatalf("got %q", got)
	}
	if got := LockedVersion(lock, "absent"); got != "" {
		t.Fatalf("got %q", got)
	}
}

func TestLastJSONAndSupports(t *testing.T) {
	info, err := LastJSON([]byte("noise\n{\"tool\":\"ppmd-bench\",\"impls\":{\"ppmd-rust\":{\"decode-7z\":true},\"ppmd-turbo\":{\"decode-7z\":false}}}\n"))
	if err != nil {
		t.Fatal(err)
	}
	driver := Driver{Info: info}
	if !driver.Supports("ppmd-rust", "decode-7z") || driver.Supports("ppmd-turbo", "decode-7z") || driver.Supports("ppmd-rust", "encode-7z") {
		t.Fatalf("Supports misread %v", info)
	}
	if _, err := LastJSON([]byte("not json\n")); err == nil {
		t.Fatal("want an error")
	}
}
