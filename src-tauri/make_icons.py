"""生成 FTPSync 应用图标（纯标准库实现，无第三方依赖）。

输出：
  icons/32x32.png, 128x128.png, 128x128@2x.png, icon.png(512)
  icons/icon.ico  （多尺寸 16/32/48/64/128/256，BMP 条目，兼容 NSIS）

实现要点：先在 768x768 的母版上做扫描线光栅化（768 是 16/32/48/64/128/256 的公倍数，
保证降采样倍率为整数），再盒式降采样得到各尺寸，天然获得抗锯齿效果。
"""

import math
import os
import struct
import zlib

MASTER = 768
C1 = (0x2F, 0x6F, 0xED)  # 蓝
C2 = (0x14, 0xB8, 0xA6)  # 青
OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "icons")

# ---------------------------------------------------------------- 画布

W = MASTER
buf = bytearray(W * W * 4)


def build_gradient():
    """预计算对角线性渐变的整幅 RGBA 字节流，避免逐像素浮点运算。"""
    g = bytearray(W * W * 4)
    span = 2 * (W - 1)
    for y in range(W):
        base = y * W * 4
        y2 = y
        for x in range(W):
            t = (x + y2) / span
            i = base + x * 4
            g[i] = int(C1[0] + (C2[0] - C1[0]) * t)
            g[i + 1] = int(C1[1] + (C2[1] - C1[1]) * t)
            g[i + 2] = int(C1[2] + (C2[2] - C1[2]) * t)
            g[i + 3] = 255
    return g


def fill_rounded_square(radius):
    """把整幅画布裁剪成圆角矩形，填充对角渐变。"""
    grad = build_gradient()
    r = radius
    for y in range(W):
        if y < r:
            dy = r - y
            inset = r - math.sqrt(max(0.0, r * r - dy * dy))
        elif y >= W - r:
            dy = y - (W - 1 - r)
            inset = r - math.sqrt(max(0.0, r * r - dy * dy))
        else:
            inset = 0.0
        left = int(math.ceil(inset))
        right = int(math.floor(W - 1 - inset))
        if right < left:
            continue
        row = y * W * 4
        buf[row + left * 4 : row + (right + 1) * 4] = grad[
            row + left * 4 : row + (right + 1) * 4
        ]


def fill_polygon(points, color):
    """扫描线填充凸/凹多边形（奇偶规则）。"""
    ys = [p[1] for p in points]
    y0 = max(0, int(math.floor(min(ys))))
    y1 = min(W - 1, int(math.ceil(max(ys))))
    n = len(points)
    for y in range(y0, y1 + 1):
        yc = y + 0.5
        xs = []
        for i in range(n):
            ax, ay = points[i]
            bx, by = points[(i + 1) % n]
            if ay == by:
                continue
            if (ay <= yc < by) or (by <= yc < ay):
                t = (yc - ay) / (by - ay)
                xs.append(ax + (bx - ax) * t)
        if not xs:
            continue
        xs.sort()
        for k in range(0, len(xs) - 1, 2):
            xl = int(math.ceil(xs[k] - 0.5))
            xr = int(math.floor(xs[k + 1] - 0.5))
            if xr < xl:
                continue
            xl = max(0, xl)
            xr = min(W - 1, xr)
            row = y * W * 4
            for x in range(xl, xr + 1):
                i = row + x * 4
                buf[i] = color[0]
                buf[i + 1] = color[1]
                buf[i + 2] = color[2]
                buf[i + 3] = 255


def draw_arrows():
    """两根反向箭头，构成同步符号。"""
    white = (255, 255, 255)

    def arrow(cx_y, x0, x1, x2, h, H, mirror=False):
        pts = [
            (x0, cx_y - h),
            (x1, cx_y - h),
            (x1, cx_y - H),
            (x2, cx_y),
            (x1, cx_y + H),
            (x1, cx_y + h),
            (x0, cx_y + h),
        ]
        if mirror:
            pts = [(W - 1 - px, py) for (px, py) in pts]
            pts.reverse()
        fill_polygon(pts, white)

    # 上方箭头向右
    arrow(
        cx_y=0.372 * W,
        x0=0.205 * W,
        x1=0.585 * W,
        x2=0.795 * W,
        h=0.050 * W,
        H=0.122 * W,
    )
    # 下方箭头向左
    arrow(
        cx_y=0.628 * W,
        x0=0.205 * W,
        x1=0.585 * W,
        x2=0.795 * W,
        h=0.050 * W,
        H=0.122 * W,
        mirror=True,
    )


def downsample(size):
    """盒式降采样到 size x size，返回 RGBA bytearray。"""
    f = W // size
    out = bytearray(size * size * 4)
    area = f * f
    for oy in range(size):
        for ox in range(size):
            r = g = b = a = 0
            for dy in range(f):
                row = (oy * f + dy) * W * 4
                for dx in range(f):
                    i = row + (ox * f + dx) * 4
                    r += buf[i]
                    g += buf[i + 1]
                    b += buf[i + 2]
                    a += buf[i + 3]
            o = (oy * size + ox) * 4
            out[o] = r // area
            out[o + 1] = g // area
            out[o + 2] = b // area
            out[o + 3] = a // area
    return out


# ---------------------------------------------------------------- 编码


def png_bytes(size, rgba):
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)  # filter type 0
        raw += rgba[y * stride : (y + 1) * stride]

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def bmp_entry(size, rgba):
    """ICO 内嵌的 32bpp BMP：BITMAPINFOHEADER + 自下而上的 BGRA + AND 掩码。"""
    header = struct.pack(
        "<IiiHHIIiiII",
        40,          # biSize
        size,        # biWidth
        size * 2,    # biHeight（XOR + AND 两段）
        1,           # biPlanes
        32,          # biBitCount
        0,           # biCompression = BI_RGB
        size * size * 4,
        0, 0, 0, 0,
    )
    xor = bytearray()
    for y in range(size - 1, -1, -1):
        row = y * size * 4
        for x in range(size):
            i = row + x * 4
            xor += bytes((rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]))
    mask_stride = ((size + 31) // 32) * 4
    and_mask = b"\x00" * (mask_stride * size)
    return header + bytes(xor) + and_mask


def ico_bytes(entries):
    """entries: [(size, rgba), ...]"""
    n = len(entries)
    dir_header = struct.pack("<HHH", 0, 1, n)
    dir_entries = b""
    offset = 6 + 16 * n
    blobs = []
    for size, rgba in entries:
        data = bmp_entry(size, rgba)
        dir_entries += struct.pack(
            "<BBBBHHII",
            0 if size >= 256 else size,
            0 if size >= 256 else size,
            0, 0, 1, 32, len(data), offset,
        )
        offset += len(data)
        blobs.append(data)
    return dir_header + dir_entries + b"".join(blobs)


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    print("[1/4] 光栅化母版 %dx%d ..." % (MASTER, MASTER))
    fill_rounded_square(radius=int(0.222 * W))
    draw_arrows()

    print("[2/4] 降采样 ...")
    cache = {}
    for s in (16, 32, 48, 64, 128, 256, 512):
        cache[s] = downsample(s)
        print("      - %dx%d" % (s, s))

    print("[3/4] 写出 PNG ...")
    png_map = {
        "32x32.png": 32,
        "128x128.png": 128,
        "128x128@2x.png": 256,
        "icon.png": 512,
    }
    for name, s in png_map.items():
        with open(os.path.join(OUT_DIR, name), "wb") as f:
            f.write(png_bytes(s, cache[s]))
        print("      -", name)

    print("[4/4] 写出 icon.ico ...")
    with open(os.path.join(OUT_DIR, "icon.ico"), "wb") as f:
        f.write(
            ico_bytes(
                [(s, cache[s]) for s in (16, 32, 48, 64, 128, 256)]
            )
        )
    print("完成 ->", OUT_DIR)


if __name__ == "__main__":
    main()
