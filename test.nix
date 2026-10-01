{ pkgs, btrmaps }:
pkgs.testers.runNixOSTest {
  name = "btrmaps";
  nodes.machine = {
    virtualisation.emptyDiskImages = [ 1024 ];
    # sway needs a GPU device; it renders with pixman and egui with llvmpipe.
    virtualisation.qemu.options = [ "-vga none -device virtio-gpu-pci" ];
    hardware.graphics.enable = true;
    environment.systemPackages = [
      btrmaps
      pkgs.btrfs-progs
      pkgs.grim
    ];
    environment.variables = {
      SWAYSOCK = "/tmp/sway-ipc.sock";
      WLR_RENDERER = "pixman";
      LIBGL_ALWAYS_SOFTWARE = "1";
    };
    programs.sway.enable = true;
    # A virtual mouse, to click through the viewer.
    programs.ydotool.enable = true;
    users.users.alice = {
      isNormalUser = true;
      extraGroups = [ "wheel" ];
      password = "hunter2";
    };
    services.getty.autologinUser = "alice";
    programs.bash.loginShellInit = ''
      if [ "$(tty)" = /dev/tty1 ]; then exec sway; fi
    '';
  };
  testScript = ''
    import json

    def mib(name, n):
        machine.succeed(f"dd if=/dev/urandom of={name} bs=1M count={n} status=none && sync")

    machine.succeed(
        "mkfs.btrfs -q /dev/vdb",
        "mkdir -p /top /root-subvol",
        "mount /dev/vdb /top",
        "btrfs subvolume create /top/@",
    )
    mib("/top/@/snapped", 24)
    machine.succeed("btrfs subvolume snapshot -r /top/@ /top/@snap")
    mib("/top/@/unique", 24)
    mib("/top/@/big", 24)
    machine.succeed("cp --reflink=always /top/@/big /top/@/big-reflink && sync")
    mib("/top/@/hard1", 24)
    machine.succeed("ln /top/@/hard1 /top/@/hard2")
    mib("/top/@/overwritten", 24)
    machine.succeed("dd if=/dev/urandom of=/top/@/overwritten bs=1M seek=8 count=8 conv=notrunc status=none && sync")
    mib("/top/top-level-file", 8)
    machine.succeed(
        "mount -o remount,compress=zstd /top",
        "{ yes compressible-line || true; } | head -c 32M > /top/@/text && sync",
    )

    # Point it at a non-top-level subvolume so it has to mount the top level itself.
    machine.succeed("mount -o subvol=@ /dev/vdb /root-subvol")
    # Let the test user delete in @, for the delete-from-the-menu check below.
    machine.succeed("chmod 777 /root-subvol")
    # The helper answers probe requests on stdin; without any it sends its header and ends.
    def scan(*requests):
        lines = "\n".join(json.dumps({"t": "probe", "positions": p}) for p in requests)
        machine.succeed(f"cat > /tmp/requests.jsonl << 'EOF'\n{lines}\nEOF")
        out = machine.succeed("btrmaps scan /root-subvol < /tmp/requests.jsonl")
        return [json.loads(l) for l in out.splitlines()]

    header = scan()[0]
    assert header["t"] == "header" and header["total"] > 0, header
    machine.succeed("test -z \"$(ls /tmp | grep btrmaps-)\"")
    machine.succeed("! grep -q /tmp/btrmaps- /proc/mounts")

    # An even grid of positions, like the window's sweep, in two requests.
    total, n = header["total"], 8192
    grid = [i * total // n + total // (2 * n) for i in range(n)]
    msgs = scan(grid[: n // 2], grid[n // 2 :])
    sets = [m for m in msgs if m["t"] == "set"]
    assert [s["id"] for s in sets] == list(range(len(sets))), "set ids not dense and ordered"
    seen = set()
    answers = []
    for m in msgs:
        if m["t"] == "set":
            seen.add(m["id"])
        if m["t"] == "runs":
            assert all(r[2] in seen for r in m["runs"]), "run before its set"
            answers += m["runs"]
    assert len(answers) == n, f"one answer per position: {len(answers)}"
    for pos, (start, length, *_) in zip(grid, answers):
        assert start <= pos < start + length, f"run {start}+{length} misses {pos}"

    # Each position's answer, then sizes by counting positions.
    final = [(sets[r[2]], r[3], r[4], r[5]) for r in answers]
    by_key = {}
    for s, _, _, _ in final:
        key = (s["kind"], tuple(s.get("paths", [])))
        by_key[key] = by_key.get(key, 0) + total / n
    for key, size in sorted(by_key.items(), key=lambda kv: -kv[1]):
        print(f"{int(size) >> 20:6} MiB  {key}")

    def size(kind, *files):
        return int(by_key.get((kind, tuple(sorted(files))), 0)) >> 20

    def near(got, want, what):
        assert abs(got - want) <= want * 0.15 + 1, f"{what}: {got} MiB, expected ~{want}"

    near(size("data", "@/unique"), 24, "unique file")
    near(size("data", "@/snapped", "@snap/snapped"), 24, "snapshot-shared file")
    near(size("data", "@/big", "@/big-reflink"), 24, "reflinked pair")
    near(size("data", "@/hard1", "@/hard2"), 24, "hardlinked pair")
    near(size("data", "@/overwritten"), 24, "overwritten file")
    near(size("unreachable", "@/overwritten"), 8, "overwritten extent tail")
    near(size("data", "top-level-file"), 8, "top-level file")
    assert size("free") > 0, "no free space found"
    assert size("metadata") > 0, "no metadata found"
    assert not any(s["kind"] == "error" for s in sets), "probe errors"

    def the_set(*files):
        return next(s for s in sets if s.get("paths") == list(files))

    snapped = the_set("@/snapped", "@snap/snapped")
    assert (snapped["files"], snapped["subvolumes"]) == (2, 2), snapped
    hard = the_set("@/hard1", "@/hard2")
    assert (hard["files"], hard["subvolumes"], hard["path_count"]) == (1, 1, 2), hard

    def answers_of(*files):
        return [(a, r, g) for s, a, r, g in final if s.get("paths") == list(files) and s["kind"] == "data"]

    text = answers_of("@/text")
    assert text and all(a == "zstd" and r > 500 for a, r, _ in text), text
    plain = answers_of("@/unique")
    assert plain and all(a == "none" and r == 100 for a, r, _ in plain), plain
    assert all(a == "unknown" for s, a, _, _ in final if s["kind"] in ("free", "metadata", "system"))

    # Ages: every extent's generation is known, later writes have later generations,
    # and the calibration btrfs records is ordered and ends now.
    def generations(*files):
        return {g for _, _, g in answers_of(*files)}
    assert min(generations("@/text")) > max(generations("@/unique")) > max(generations("@/snapped", "@snap/snapped")) > 0
    cal = header["calibration"]
    assert len(cal) >= 2 and cal == sorted(cal), cal
    assert all(t1 >= t0 for (_, t0), (_, t1) in zip(cal, cal[1:])), cal

    # Runs cover whatever is known to share the position: a data answer its extent's
    # used part, a free answer the stretch up to the next extent.
    def run_of(kind, *files):
        return next(r for r, (s, _, _, _) in zip(answers, final) if s["kind"] == kind and s.get("paths", []) == list(files))
    assert run_of("data", "@/unique")[1] >= 1 << 20, "a data answer covers its extent"
    assert run_of("free")[1] > 4096, "a free answer covers its stretch"

    # The app: launch it, scan the preselected filesystem through sudo.
    machine.wait_for_file("/tmp/sway-ipc.sock")
    alice = "su - alice -c "
    window = alice + "'swaymsg -t get_tree' | grep -q '\"app_id\": \"btrmaps\"'"
    ydotool = "YDOTOOL_SOCKET=/run/ydotoold/socket ydotool "
    machine.succeed(alice + "'swaymsg input type:pointer accel_profile flat'")

    def shot(name):
        machine.succeed(alice + f"'XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 grim -c /tmp/{name}.png'")
        machine.copy_from_machine(f"/tmp/{name}.png", "")

    def point(x, y):
        machine.succeed(ydotool + f"mousemove --absolute -x {x} -y {y}")
        machine.sleep(1)

    def click(x, y):
        point(x, y)
        machine.succeed(ydotool + "click 0xC0")
        machine.sleep(2)

    machine.succeed(alice + "'swaymsg exec \"env BTRMAPS_FRAMES=1 btrmaps 2>/tmp/frames.log\"'")
    machine.wait_until_succeeds(window, timeout=60)
    machine.sleep(3)
    shot("start")
    # A narrow window: the mode buttons become a menu.
    machine.succeed(alice + "'swaymsg [app_id=btrmaps] floating enable, resize set 800 600, move position 0 30'")
    machine.sleep(2)
    shot("narrow")
    machine.succeed(alice + "'swaymsg [app_id=btrmaps] floating disable'")
    machine.sleep(2)
    # The only filesystem is preselected; the big button starts the scan.
    click(640, 420)  # Scan
    machine.sleep(2)
    shot("password")
    # A wrong password is turned down at once and asked for again, not left hanging.
    machine.succeed(ydotool + "type wrong")
    machine.succeed(ydotool + "key 28:1 28:0")  # Enter
    machine.sleep(6)
    shot("wrong-password")
    machine.succeed(ydotool + "type hunter2")
    machine.succeed(ydotool + "key 28:1 28:0")  # Enter
    # sudo accepted the typed password and ran the scan as root.
    machine.wait_until_succeeds("journalctl -t sudo | grep -q 'alice : .*COMMAND=.*btrmaps scan'", timeout=30)
    machine.sleep(20)
    machine.succeed(window)
    point(640, 790)
    shot("curve")
    # Drag the map around for a few seconds, timing frames.
    point(450, 430)
    machine.succeed(ydotool + "click 0x40")
    for i in range(40):
        machine.succeed(ydotool + f"mousemove -x {8 if i < 20 else -8} -y {5 if i % 10 < 5 else -5}")
    machine.succeed(ydotool + "click 0x80")
    machine.sleep(3)
    # Resizing shows more or less of the map at the same scale; it doesn't rescale it.
    machine.succeed(alice + "'swaymsg [app_id=btrmaps] floating enable, resize set 900 600, move position 0 30'")
    machine.sleep(3)
    shot("resized")
    machine.succeed(alice + "'swaymsg [app_id=btrmaps] floating disable'")
    machine.sleep(3)
    # Zoomed in a little, the map hangs off every edge and must stay undistorted.
    for _ in range(3):
        machine.succeed(ydotool + "key 13:1 13:0")  # =, zoom in
    machine.sleep(3)
    shot("zoomed")
    # Zoom far past the scan's detail: the view refines itself from exact probes.
    for _ in range(8):
        machine.succeed(ydotool + "key 13:1 13:0")  # =, zoom in
    machine.sleep(6)
    shot("deep")
    for _ in range(8):
        machine.succeed(ydotool + "key 12:1 12:0")  # -, zoom back out
    machine.sleep(2)
    point(600, 500)  # free space: described in the side pane, but nothing dims
    shot("free-hover")
    point(250, 330)  # the unique file: described, but hovering alone dims nothing
    shot("hover")
    machine.succeed(ydotool + "key 42:1")  # hold Shift: everything else dims
    machine.sleep(1)
    shot("peek")
    machine.succeed(ydotool + "key 42:0")
    machine.succeed(ydotool + "click 0xC1")  # right button
    machine.sleep(2)
    shot("menu")
    click(295, 436)  # Delete file…
    shot("confirm")
    click(453, 453)  # Delete
    # The file the menu was opened on is gone; its neighbours are not.
    machine.wait_until_fails("test -e /top/@/unique", timeout=10)
    machine.succeed("test -e /top/@/big && test -e /top/@/snapped")
    shot("deleted")
    # Modes recolor through the GPU's tables, without touching tiles.
    click(684, 68)  # Compression
    shot("compression")
    click(755, 68)  # Age
    shot("age")
    print(machine.succeed("cat /tmp/frames.log"))
  '';
}
