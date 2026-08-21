// Project-local `tui` tool: run and debug omp-tui apps (examples or cargo
// bins) headlessly. Each session spawns the target on a Bun-native PTY — a
// real controlling terminal, so capability probes, SIGWINCH resizes, and
// immediate-mode hosts all behave as in production — with `OMP_TUI_DEBUG`
// pointed at a unix socket every omp-tui host serves. The wire speaks the
// crate's `TerminalEvent`: injected input rides the terminal's own event
// mailbox, screenshots answer from the renderer's last paint, and
// `frame`/`tree`/`values` are mailbox queries answered by `App` hosts
// (immediate-mode hosts let them time out server-side).
//
// A built-in VT screen emulator tracks every PTY byte, so `screen` (and the
// socketless `text` fallback) render any app's display as plain text — no
// escape decoding required — and input ops echo an after-screenshot.
//
// Sessions live in this module for the lifetime of the agent session; the
// child and its terminal are torn down on `stop` or shutdown.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import * as net from "node:net";
import type * as CanvasModule from "@napi-rs/canvas";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** Minimal slice of the host schema builder (`omp.zod`) this tool uses. */
interface Schema {
	describe(text: string): Schema;
	optional(): Schema;
}
interface SchemaBuilder {
	object(shape: Record<string, Schema>): Schema;
	string(): Schema;
	boolean(): Schema;
	number(): Schema;
	array(item: Schema): Schema;
}
interface ExecResult {
	stdout: string;
	stderr: string;
	code: number | null;
	killed?: boolean;
}
/** Minimal slice of `CustomToolAPI` this tool uses. */
interface ToolHost {
	cwd: string;
	zod: SchemaBuilder;
	exec(
		command: string,
		args: string[],
		options?: { cwd?: string; signal?: AbortSignal },
	): Promise<ExecResult>;
}
interface ToolUpdate {
	content: { type: "text"; text: string }[];
	details?: Record<string, unknown>;
}
interface ToolResult {
	content: (
		| { type: "text"; text: string }
		| { type: "image"; data: string; mimeType: string }
	)[];
	details?: Record<string, unknown>;
}

interface TuiParams {
	op:
		| "start"
		| "stop"
		| "list"
		| "text"
		| "screen"
		| "frame"
		| "tree"
		| "values"
		| "info"
		| "keys"
		| "type"
		| "paste"
		| "mouse"
		| "send"
		| "resize"
		| "raw"
		| "shot";
	name?: string;
	example?: string;
	bin?: string;
	args?: string[];
	rows?: number;
	cols?: number;
	build?: boolean;
	keys?: string;
	text?: string;
	x?: number;
	y?: number;
	action?: string;
	peek?: number;
	clear?: boolean;
	quiet?: boolean;
	timeout?: number;
}

// ─── VT screen emulator ──────────────────────────────────────────────────────

/** Whether a code point occupies two terminal columns (common CJK/emoji ranges). */
function wide(cp: number): boolean {
	return (
		(cp >= 0x1100 && cp <= 0x115f) ||
		(cp >= 0x2e80 && cp <= 0xa4cf) ||
		(cp >= 0xac00 && cp <= 0xd7a3) ||
		(cp >= 0xf900 && cp <= 0xfaff) ||
		(cp >= 0xfe30 && cp <= 0xfe4f) ||
		(cp >= 0xff00 && cp <= 0xff60) ||
		(cp >= 0xffe0 && cp <= 0xffe6) ||
		(cp >= 0x1f300 && cp <= 0x1faff) ||
		(cp >= 0x20000 && cp <= 0x3fffd)
	);
}

/** Whether a code point occupies no terminal column (combining marks, ZWJ, variation selectors). */
function zeroWidth(cp: number): boolean {
	return (
		(cp >= 0x0300 && cp <= 0x036f) ||
		(cp >= 0x0483 && cp <= 0x0489) ||
		(cp >= 0x1ab0 && cp <= 0x1aff) ||
		(cp >= 0x1dc0 && cp <= 0x1dff) ||
		(cp >= 0x200b && cp <= 0x200f) ||
		cp === 0x2060 ||
		(cp >= 0x20d0 && cp <= 0x20ff) ||
		(cp >= 0xfe00 && cp <= 0xfe0f) ||
		(cp >= 0xfe20 && cp <= 0xfe2f) ||
		(cp >= 0xe0100 && cp <= 0xe01ef)
	);
}

/** Style attribute bits carried per cell. */
const BOLD = 1;
const DIM = 2;
const ITALIC = 4;
const UNDERLINE = 8;
const REVERSE = 16;
const STRIKE = 32;

/** Interned SGR paint state shared by every cell painted under it (colors 0xRRGGBB). */
interface Style {
	fg: number | null;
	bg: number | null;
	flags: number;
}

/** One rendered cell: glyph (empty = wide-char continuation) + style (null = default). */
interface Cell {
	ch: string;
	st: Style | null;
}

const BLANK: Cell = { ch: " ", st: null };

/** xterm's default 16-color palette. */
const PALETTE16 = [
	0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5,
	0x7f7f7f, 0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
];

/** xterm 256-color index → 0xRRGGBB (16 basic, 6×6×6 cube, 24 grays). */
function color256(index: number): number {
	if (index < 16) return PALETTE16[index] ?? PALETTE16[0];
	let r: number;
	let g: number;
	let b: number;
	if (index < 232) {
		const cube = index - 16;
		r = Math.floor(cube / 36);
		g = Math.floor(cube / 6) % 6;
		b = cube % 6;
		r = r === 0 ? 0 : 55 + 40 * r;
		g = g === 0 ? 0 : 55 + 40 * g;
		b = b === 0 ? 0 : 55 + 40 * b;
	} else {
		r = g = b = 8 + 10 * (index - 232);
	}
	return (r << 16) | (g << 8) | b;
}

// ─── PNG rasterizer ──────────────────────────────────────────────────────────
//
// `shot` rasterizes the emulator grid with @napi-rs/canvas (prebuilt Skia):
// real system fonts with an explicit fallback stack (Menlo → nerd font →
// braille → CJK → emoji), so icons, spinners, and wide text render as a
// terminal would. Powerline separators (U+E0B0–E0B3) are drawn as vector
// paths because font metrics leave gaps at cell edges. Imported lazily so a
// missing install degrades only `shot`, not the tool.

/** Cell pixel geometry. */
const CW = 16;
const CH = 32;
/** Font size whose Menlo advance (~0.602 em) fits the 16px cell. */
const FONT_PX = 26;
/** Baseline offset inside a cell (Menlo 26px ascent ≈ 24.2px). */
const BASELINE = 24;
const DEF_FG = 0xd4d4d4;
const DEF_BG = 0x101418;

/** Lazily loaded canvas module + resolved font-family stack. */
let canvasApi: { create: typeof CanvasModule.createCanvas; stack: string } | null = null;

/**
 * Loads @napi-rs/canvas and builds the fallback font stack from the fonts
 * actually installed: Menlo, first nerd font, braille, CJK, emoji.
 */
async function loadCanvas() {
	if (canvasApi) return canvasApi;
	let mod: typeof CanvasModule;
	try {
		// Dynamic import: the dependency is optional — a missing
		// `bun install` in .omp/tools must only disable `shot`, not fail
		// the whole tool module at load.
		mod = await import("@napi-rs/canvas");
	} catch (error) {
		throw new Error(
			`shot needs @napi-rs/canvas — run \`bun install\` in .omp/tools (${error})`,
		);
	}
	const families = mod.GlobalFonts.families.map((entry) => entry.family);
	const stack = ["Menlo"];
	const nerd =
		families.find((family) => /nerd font mono/i.test(family)) ??
		families.find((family) => /nerd font/i.test(family));
	if (nerd) stack.push(nerd);
	for (const wanted of [
		"Apple Braille",
		"PingFang SC",
		"Hiragino Sans",
		"Noto Sans CJK SC",
		"Apple Color Emoji",
		"Noto Color Emoji",
	]) {
		if (families.includes(wanted)) stack.push(wanted);
	}
	canvasApi = {
		create: mod.createCanvas,
		stack: stack.map((family) => `"${family}"`).join(", "),
	};
	return canvasApi;
}

/** 0xRRGGBB → CSS hex color. */
function css(rgb: number): string {
	return `#${rgb.toString(16).padStart(6, "0")}`;
}

/** Per-channel linear blend of two 0xRRGGBB colors. */
function lerpColor(from: number, to: number, t: number): number {
	const channel = (shift: number) => {
		const a = (from >> shift) & 0xff;
		const b = (to >> shift) & 0xff;
		return Math.round(a + (b - a) * t) << shift;
	};
	return channel(16) | channel(8) | channel(0);
}

/**
 * Minimal VT100/xterm cell-grid emulator fed every PTY byte. Text and SGR
 * styling are both tracked, so any session — omp-tui host or not — can be
 * read as plain text (`snapshot`) or rasterized to pixels (`png`). Backs the
 * `screen`/`shot` ops, the socketless `text` fallback, and the
 * after-screenshot on input ops.
 */
class Screen {
	cols: number;
	rows: number;
	/** Lines scrolled off the top of the primary screen (capped at 2000). */
	scrollback: string[] = [];
	altActive = false;
	cursorVisible = true;
	private grid: Cell[][];
	private altGrid: Cell[][];
	private current: Style | null = null;
	private x = 0;
	private y = 0;
	private savedX = 0;
	private savedY = 0;
	private top = 0;
	private bottom: number;
	private wrapPending = false;
	private state: "ground" | "esc" | "csi" | "osc" | "str" | "charset" = "ground";
	private csiBuf = "";
	private utf8: number[] = [];
	private utf8Need = 0;

	constructor(cols: number, rows: number) {
		this.cols = cols;
		this.rows = rows;
		this.bottom = rows - 1;
		this.grid = Screen.blank(cols, rows);
		this.altGrid = Screen.blank(cols, rows);
	}

	private static blank(cols: number, rows: number): Cell[][] {
		return Array.from({ length: rows }, () => new Array<Cell>(cols).fill(BLANK));
	}

	private active(): Cell[][] {
		return this.altActive ? this.altGrid : this.grid;
	}

	/** Blank cell for erases: carries the current background (bce), else default. */
	private eraseCell(): Cell {
		const bg = this.current?.bg;
		return bg ? { ch: " ", st: { fg: null, bg, flags: 0 } } : BLANK;
	}

	feed(chunk: Buffer) {
		for (const byte of chunk) this.step(byte);
	}

	/** Plain-text screen: header, optional scrollback tail, then the viewport. */
	snapshot(history = 0): string {
		const out = [
			`── screen ${this.cols}x${this.rows}` +
				`${this.altActive ? ", alt screen" : ""}` +
				`, cursor=[${this.x},${this.y}]${this.cursorVisible ? "" : " hidden"}` +
				`, scrollback=${this.scrollback.length} ──`,
		];
		for (const line of history > 0 ? this.scrollback.slice(-history) : []) {
			out.push(`┆${line}`);
		}
		for (const row of this.active()) {
			out.push(`│${row.map((cell) => cell.ch).join("").replace(/ +$/, "")}`);
		}
		return out.join("\n");
	}

	/** Rasterizes the viewport to a PNG via Skia with real system fonts. */
	async png(): Promise<Buffer> {
		const { create, stack } = await loadCanvas();
		const width = this.cols * CW;
		const height = this.rows * CH;
		const canvas = create(width, height);
		const ctx = canvas.getContext("2d");
		ctx.fillStyle = css(DEF_BG);
		ctx.fillRect(0, 0, width, height);
		const grid = this.active();
		// Resolve each cell's paint once; draw backgrounds before any text so
		// glyph overhang (italics, wide fallback) never sits under a
		// neighbor's background.
		const cells: { px: number; py: number; ch: string; fg: number; flags: number; span: number }[] =
			[];
		for (let y = 0; y < this.rows; y++) {
			const row = grid[y];
			for (let x = 0; x < this.cols; x++) {
				const cell = row[x];
				// Wide-char continuations are drawn by their lead cell.
				if (cell.ch === "") continue;
				const st = cell.st;
				let fg = st?.fg ?? DEF_FG;
				let bg = st?.bg ?? null;
				if (st && st.flags & REVERSE) {
					const swap = fg;
					fg = bg ?? DEF_BG;
					bg = swap;
				}
				if (st && st.flags & DIM) fg = lerpColor(fg, bg ?? DEF_BG, 0.45);
				const span = x + 1 < this.cols && row[x + 1].ch === "" ? 2 : 1;
				const px = x * CW;
				const py = y * CH;
				if (bg !== null) {
					ctx.fillStyle = css(bg);
					ctx.fillRect(px, py, span * CW, CH);
				}
				cells.push({ px, py, ch: cell.ch, fg, flags: st?.flags ?? 0, span });
			}
		}
		for (const { px, py, ch, fg, flags, span } of cells) {
			ctx.fillStyle = css(fg);
			if (flags & UNDERLINE) ctx.fillRect(px, py + 28, span * CW, 2);
			if (flags & STRIKE) ctx.fillRect(px, py + 15, span * CW, 2);
			if (ch === " ") continue;
			const cp = ch.codePointAt(0) ?? 0;
			if (cp >= 0xe0b0 && cp <= 0xe0b3) {
				// Powerline separators as vector paths: font metrics leave
				// gaps at cell edges, which reads as broken bands.
				ctx.beginPath();
				if (cp <= 0xe0b1) {
					ctx.moveTo(px, py);
					ctx.lineTo(px + CW, py + CH / 2);
					ctx.lineTo(px, py + CH);
				} else {
					ctx.moveTo(px + CW, py);
					ctx.lineTo(px, py + CH / 2);
					ctx.lineTo(px + CW, py + CH);
				}
				if (cp === 0xe0b0 || cp === 0xe0b2) {
					ctx.closePath();
					ctx.fill();
				} else {
					ctx.lineWidth = 2;
					ctx.strokeStyle = css(fg);
					ctx.stroke();
				}
				continue;
			}
			ctx.font =
				`${flags & ITALIC ? "italic " : ""}${flags & BOLD ? "bold " : ""}` +
				`${FONT_PX}px ${stack}`;
			ctx.fillText(ch, px, py + BASELINE);
		}
		if (this.cursorVisible) {
			ctx.globalAlpha = 0.4;
			ctx.fillStyle = "#ffffff";
			ctx.fillRect(this.x * CW, this.y * CH, CW, CH);
			ctx.globalAlpha = 1;
		}
		return canvas.toBuffer("image/png");
	}

	resize(cols: number, rows: number) {
		const fit = (grid: Cell[][]) => {
			grid.length = Math.min(grid.length, rows);
			while (grid.length < rows) grid.push(new Array<Cell>(cols).fill(BLANK));
			for (const row of grid) {
				row.length = Math.min(row.length, cols);
				while (row.length < cols) row.push(BLANK);
			}
		};
		this.cols = cols;
		this.rows = rows;
		fit(this.grid);
		fit(this.altGrid);
		this.top = 0;
		this.bottom = rows - 1;
		this.x = Math.min(this.x, cols - 1);
		this.y = Math.min(this.y, rows - 1);
		this.wrapPending = false;
	}

	private step(byte: number) {
		switch (this.state) {
			case "ground":
				if (byte === 0x1b) {
					this.utf8Need = 0;
					this.state = "esc";
				} else if (byte < 0x20 || byte === 0x7f) this.control(byte);
				else this.decode(byte);
				return;
			case "esc":
				this.escByte(byte);
				return;
			case "charset":
				this.state = "ground";
				return;
			case "csi":
				if (byte === 0x1b) this.state = "esc";
				else if (byte >= 0x40 && byte <= 0x7e) {
					this.state = "ground";
					this.csi(String.fromCharCode(byte));
				} else if (byte >= 0x20) this.csiBuf += String.fromCharCode(byte);
				else this.control(byte);
				return;
			case "osc":
				if (byte === 0x07) this.state = "ground";
				else if (byte === 0x1b) this.state = "esc";
				return;
			case "str":
				if (byte === 0x1b) this.state = "esc";
				return;
		}
	}

	private escByte(byte: number) {
		this.state = "ground";
		switch (String.fromCharCode(byte)) {
			case "[":
				this.csiBuf = "";
				this.state = "csi";
				return;
			case "]":
				this.state = "osc";
				return;
			case "P":
			case "X":
			case "^":
			case "_":
				this.state = "str";
				return;
			case "(":
			case ")":
			case "*":
			case "+":
			case "#":
			case "%":
				this.state = "charset";
				return;
			case "7":
				this.savedX = this.x;
				this.savedY = this.y;
				return;
			case "8":
				this.x = this.savedX;
				this.y = this.savedY;
				this.wrapPending = false;
				return;
			case "D":
				this.lineFeed();
				return;
			case "E":
				this.lineFeed();
				this.x = 0;
				return;
			case "M":
				this.reverseIndex();
				return;
			case "c":
				this.reset();
				return;
			default:
				return;
		}
	}

	private control(byte: number) {
		if (byte === 0x0d) {
			this.x = 0;
			this.wrapPending = false;
		} else if (byte === 0x0a || byte === 0x0b || byte === 0x0c) this.lineFeed();
		else if (byte === 0x08) {
			if (this.x > 0) this.x--;
			this.wrapPending = false;
		} else if (byte === 0x09) {
			this.x = Math.min((Math.floor(this.x / 8) + 1) * 8, this.cols - 1);
			this.wrapPending = false;
		}
	}

	private decode(byte: number) {
		if (this.utf8Need > 0) {
			if ((byte & 0xc0) === 0x80) {
				this.utf8.push(byte);
				if (this.utf8.length === this.utf8Need) {
					const text = Buffer.from(this.utf8).toString("utf8");
					this.utf8Need = 0;
					for (const ch of text) this.print(ch);
				}
				return;
			}
			this.utf8Need = 0;
		}
		if (byte < 0x80) {
			this.print(String.fromCharCode(byte));
			return;
		}
		const need = byte >= 0xf0 ? 4 : byte >= 0xe0 ? 3 : byte >= 0xc0 ? 2 : 0;
		if (need === 0) return;
		this.utf8 = [byte];
		this.utf8Need = need;
	}

	private print(ch: string) {
		const cp = ch.codePointAt(0) ?? 0;
		if (zeroWidth(cp)) {
			// Attach to the cell the cursor last wrote (skipping wide-char
			// continuation cells) so columns stay aligned.
			const row = this.active()[this.y];
			let cell = this.wrapPending ? this.cols - 1 : this.x - 1;
			while (cell > 0 && row[cell].ch === "") cell--;
			if (cell >= 0) row[cell] = { ch: row[cell].ch + ch, st: row[cell].st };
			return;
		}
		if (this.wrapPending) {
			this.wrapPending = false;
			this.x = 0;
			this.lineFeed();
		}
		const width = wide(cp) ? 2 : 1;
		if (width === 2 && this.x >= this.cols - 1) {
			this.x = 0;
			this.lineFeed();
		}
		const row = this.active()[this.y];
		row[this.x] = { ch, st: this.current };
		if (width === 2 && this.x + 1 < this.cols) {
			row[this.x + 1] = { ch: "", st: this.current };
		}
		this.x += width;
		if (this.x >= this.cols) {
			this.x = this.cols - 1;
			this.wrapPending = true;
		}
	}

	private lineFeed() {
		this.wrapPending = false;
		if (this.y === this.bottom) this.scrollUp(1);
		else if (this.y < this.rows - 1) this.y++;
	}

	private reverseIndex() {
		this.wrapPending = false;
		if (this.y === this.top) this.scrollDown(1);
		else if (this.y > 0) this.y--;
	}

	private scrollUp(n: number) {
		const grid = this.active();
		for (let i = 0; i < n; i++) {
			const removed = grid.splice(this.top, 1)[0];
			if (!this.altActive && this.top === 0) {
				this.scrollback.push(removed.map((cell) => cell.ch).join("").replace(/ +$/, ""));
				if (this.scrollback.length > 2000) this.scrollback.shift();
			}
			grid.splice(this.bottom, 0, new Array<Cell>(this.cols).fill(BLANK));
		}
	}

	private scrollDown(n: number) {
		const grid = this.active();
		for (let i = 0; i < n; i++) {
			grid.splice(this.bottom, 1);
			grid.splice(this.top, 0, new Array<Cell>(this.cols).fill(BLANK));
		}
	}

	private eraseDisplay(kind: number) {
		if (kind === 3) {
			this.scrollback = [];
			return;
		}
		const grid = this.active();
		const cell = this.eraseCell();
		if (kind === 2) {
			for (const row of grid) row.fill(cell);
		} else if (kind === 1) {
			for (let row = 0; row < this.y; row++) grid[row].fill(cell);
			grid[this.y].fill(cell, 0, this.x + 1);
		} else {
			grid[this.y].fill(cell, this.x);
			for (let row = this.y + 1; row < this.rows; row++) grid[row].fill(cell);
		}
	}

	private eraseLine(kind: number) {
		const row = this.active()[this.y];
		const cell = this.eraseCell();
		if (kind === 2) row.fill(cell);
		else if (kind === 1) row.fill(cell, 0, this.x + 1);
		else row.fill(cell, this.x);
	}

	private mode(modes: number[], set: boolean) {
		for (const mode of modes) {
			if (mode === 25) this.cursorVisible = set;
			else if (mode === 47 || mode === 1047 || mode === 1049) {
				if (set && !this.altActive) {
					this.savedX = this.x;
					this.savedY = this.y;
					this.altActive = true;
					this.altGrid = Screen.blank(this.cols, this.rows);
					this.top = 0;
					this.bottom = this.rows - 1;
					this.x = 0;
					this.y = 0;
				} else if (!set && this.altActive) {
					this.altActive = false;
					this.top = 0;
					this.bottom = this.rows - 1;
					this.x = this.savedX;
					this.y = this.savedY;
				}
			}
		}
	}

	private reset() {
		this.grid = Screen.blank(this.cols, this.rows);
		this.altGrid = Screen.blank(this.cols, this.rows);
		this.altActive = false;
		this.current = null;
		this.x = this.y = this.savedX = this.savedY = 0;
		this.top = 0;
		this.bottom = this.rows - 1;
		this.cursorVisible = true;
		this.wrapPending = false;
	}

	/** Applies one SGR parameter string (handles `;` params and `:` subparams). */
	private sgr(raw: string) {
		let fg = this.current?.fg ?? null;
		let bg = this.current?.bg ?? null;
		let flags = this.current?.flags ?? 0;
		const params = raw === "" ? ["0"] : raw.split(";");
		for (let i = 0; i < params.length; i++) {
			const subs = params[i].split(":");
			const code = Number.parseInt(subs[0], 10) || 0;
			switch (code) {
				case 0:
					fg = bg = null;
					flags = 0;
					break;
				case 1:
					flags |= BOLD;
					break;
				case 2:
					flags |= DIM;
					break;
				case 3:
					flags |= ITALIC;
					break;
				case 4:
					if (subs[1] === "0") flags &= ~UNDERLINE;
					else flags |= UNDERLINE;
					break;
				case 7:
					flags |= REVERSE;
					break;
				case 9:
					flags |= STRIKE;
					break;
				case 22:
					flags &= ~(BOLD | DIM);
					break;
				case 23:
					flags &= ~ITALIC;
					break;
				case 24:
					flags &= ~UNDERLINE;
					break;
				case 27:
					flags &= ~REVERSE;
					break;
				case 29:
					flags &= ~STRIKE;
					break;
				case 39:
					fg = null;
					break;
				case 49:
					bg = null;
					break;
				case 38:
				case 48:
				case 58: {
					// Extended color: `38;2;r;g;b` / `38;5;n` (also `:` subparam
					// forms, incl. the optional colorspace slot). 58 (underline
					// color) is consumed but dropped.
					let color: number | null = null;
					if (subs.length > 1) {
						const mode = Number.parseInt(subs[1], 10);
						if (mode === 5) color = color256(Number.parseInt(subs[2], 10) || 0);
						else if (mode === 2) {
							const from = subs.length > 5 ? 3 : 2;
							const [r, g, b] = subs
								.slice(from, from + 3)
								.map((sub) => Number.parseInt(sub, 10) || 0);
							color = (r << 16) | (g << 8) | b;
						}
					} else {
						const mode = Number.parseInt(params[++i], 10);
						if (mode === 5) color = color256(Number.parseInt(params[++i], 10) || 0);
						else if (mode === 2) {
							const r = Number.parseInt(params[++i], 10) || 0;
							const g = Number.parseInt(params[++i], 10) || 0;
							const b = Number.parseInt(params[++i], 10) || 0;
							color = (r << 16) | (g << 8) | b;
						}
					}
					if (code === 38) fg = color;
					else if (code === 48) bg = color;
					break;
				}
				default:
					if (code >= 30 && code <= 37) fg = PALETTE16[code - 30];
					else if (code >= 90 && code <= 97) fg = PALETTE16[code - 90 + 8];
					else if (code >= 40 && code <= 47) bg = PALETTE16[code - 40];
					else if (code >= 100 && code <= 107) bg = PALETTE16[code - 100 + 8];
					break;
			}
		}
		this.current = fg === null && bg === null && flags === 0 ? null : { fg, bg, flags };
	}

	private csi(final: string) {
		const buf = this.csiBuf;
		const priv = /^[?>=<]/.test(buf);
		if (final === "m") {
			if (!priv) this.sgr(buf);
			return;
		}
		const nums = (priv ? buf.slice(1) : buf)
			.split(";")
			.map((part) => Number.parseInt(part, 10));
		const arg = (index: number, fallback: number) =>
			Number.isFinite(nums[index]) && nums[index] > 0 ? nums[index] : fallback;
		this.wrapPending = false;
		switch (final) {
			case "A":
				this.y = Math.max(0, this.y - arg(0, 1));
				break;
			case "B":
				this.y = Math.min(this.rows - 1, this.y + arg(0, 1));
				break;
			case "C":
				this.x = Math.min(this.cols - 1, this.x + arg(0, 1));
				break;
			case "D":
				this.x = Math.max(0, this.x - arg(0, 1));
				break;
			case "E":
				this.y = Math.min(this.rows - 1, this.y + arg(0, 1));
				this.x = 0;
				break;
			case "F":
				this.y = Math.max(0, this.y - arg(0, 1));
				this.x = 0;
				break;
			case "G":
			case "`":
				this.x = Math.min(this.cols - 1, arg(0, 1) - 1);
				break;
			case "H":
			case "f":
				this.y = Math.min(this.rows - 1, arg(0, 1) - 1);
				this.x = Math.min(this.cols - 1, arg(1, 1) - 1);
				break;
			case "d":
				this.y = Math.min(this.rows - 1, arg(0, 1) - 1);
				break;
			case "J":
				this.eraseDisplay(nums[0] > 0 ? nums[0] : 0);
				break;
			case "K":
				this.eraseLine(nums[0] > 0 ? nums[0] : 0);
				break;
			case "L":
			case "M": {
				if (this.y < this.top || this.y > this.bottom) break;
				const grid = this.active();
				for (let i = Math.min(arg(0, 1), this.rows); i > 0; i--) {
					if (final === "L") {
						grid.splice(this.bottom, 1);
						grid.splice(this.y, 0, new Array<Cell>(this.cols).fill(BLANK));
					} else {
						grid.splice(this.y, 1);
						grid.splice(this.bottom, 0, new Array<Cell>(this.cols).fill(BLANK));
					}
				}
				break;
			}
			case "@": {
				const row = this.active()[this.y];
				const cell = this.eraseCell();
				for (let i = Math.min(arg(0, 1), this.cols); i > 0; i--) {
					row.pop();
					row.splice(this.x, 0, cell);
				}
				break;
			}
			case "P": {
				const row = this.active()[this.y];
				const cell = this.eraseCell();
				for (let i = Math.min(arg(0, 1), this.cols); i > 0; i--) {
					row.splice(this.x, 1);
					row.push(cell);
				}
				break;
			}
			case "X":
				this.active()[this.y].fill(
					this.eraseCell(),
					this.x,
					Math.min(this.cols, this.x + arg(0, 1)),
				);
				break;
			case "S":
				this.scrollUp(Math.min(arg(0, 1), this.rows));
				break;
			case "T":
				this.scrollDown(Math.min(arg(0, 1), this.rows));
				break;
			case "r":
				this.top = Math.min(this.rows - 1, arg(0, 1) - 1);
				this.bottom = Math.min(this.rows - 1, arg(1, this.rows) - 1);
				if (this.top >= this.bottom) {
					this.top = 0;
					this.bottom = this.rows - 1;
				}
				this.x = 0;
				this.y = 0;
				break;
			case "h":
			case "l":
				if (priv) this.mode(nums, final === "h");
				break;
			case "s":
				this.savedX = this.x;
				this.savedY = this.y;
				break;
			case "u":
				if (!priv) {
					this.x = this.savedX;
					this.y = this.savedY;
				}
				break;
			default:
				// Queries and anything unrecognized: no text effect.
				break;
		}
	}
}

// ─── Sessions ────────────────────────────────────────────────────────────────

interface Waiter {
	resolve(value: Record<string, unknown>): void;
	reject(error: Error): void;
	timer: NodeJS.Timeout;
}

/** The slice of `Bun.spawn`'s PTY handle this tool touches. */
interface TerminalHandle {
	write(data: string | Uint8Array): void;
	resize(cols: number, rows: number): void;
	close(): void;
}

/** The slice of Bun's PTY-backed subprocess this tool touches. */
interface Child {
	pid: number;
	exited: Promise<number>;
	terminal: TerminalHandle;
	kill(signal?: number | NodeJS.Signals): void;
}

interface Session {
	name: string;
	target: string;
	proc: Child;
	cols: number;
	rows: number;
	dir: string;
	screen: Screen;
	sock: net.Socket | null;
	sockBuf: string;
	waiters: Waiter[];
	raw: Buffer[];
	rawBytes: number;
	exit: number | null;
}

const sessions = new Map<string, Session>();

/** Appends one PTY chunk to the session's capped raw capture. */
function capture(session: Session, chunk: Buffer) {
	session.screen.feed(chunk);
	session.raw.push(chunk);
	session.rawBytes += chunk.length;
	// Cap the capture at 8 MiB, dropping the oldest chunks.
	while (session.rawBytes > 8 * 1024 * 1024 && session.raw.length > 1) {
		session.rawBytes -= session.raw[0].length;
		session.raw.shift();
	}
}

function connectSocket(session: Session, path: string, timeoutMs: number): Promise<boolean> {
	const deadline = Date.now() + timeoutMs;
	const { promise, resolve } = Promise.withResolvers<boolean>();
	const attempt = () => {
		const sock = net.createConnection(path);
		sock.on("connect", () => {
			sock.setEncoding("utf8");
			sock.on("data", (data: string) => {
				session.sockBuf += data;
				for (;;) {
					const index = session.sockBuf.indexOf("\n");
					if (index < 0) return;
					const line = session.sockBuf.slice(0, index);
					session.sockBuf = session.sockBuf.slice(index + 1);
					const waiter = session.waiters.shift();
					if (!waiter) continue;
					clearTimeout(waiter.timer);
					try {
						waiter.resolve(JSON.parse(line));
					} catch (error) {
						waiter.reject(new Error(`bad response line: ${error}`));
					}
				}
			});
			sock.on("close", () => {
				if (session.sock === sock) session.sock = null;
			});
			sock.on("error", () => {});
			session.sock = sock;
			resolve(true);
		});
		sock.on("error", () => {
			sock.destroy();
			if (Date.now() >= deadline || session.exit !== null) resolve(false);
			else setTimeout(attempt, 150);
		});
	};
	attempt();
	return promise;
}

/** Sends one debug request and awaits its response line. */
function request(
	session: Session,
	body: Record<string, unknown>,
	timeoutMs = 10_000,
): Promise<Record<string, unknown>> {
	const sock = session.sock;
	if (!sock) {
		return Promise.reject(
			new Error(
				`session "${session.name}" has no debug socket — the app is not an ` +
					"omp-tui host (or exited). screen/raw/send/resize/stop still work.",
			),
		);
	}
	const { promise, resolve, reject } = Promise.withResolvers<Record<string, unknown>>();
	const timer = setTimeout(() => {
		const index = session.waiters.findIndex((waiter) => waiter.timer === timer);
		if (index >= 0) session.waiters.splice(index, 1);
		reject(new Error(`debug request timed out: ${JSON.stringify(body)}`));
	}, timeoutMs);
	session.waiters.push({ resolve, reject, timer });
	sock.write(`${JSON.stringify(body)}\n`);
	return promise;
}

function need(session: string | undefined): Session {
	const name = session ?? "main";
	const found = sessions.get(name);
	if (!found) {
		const names = [...sessions.keys()].join(", ") || "none";
		throw new Error(`no session "${name}" (running: ${names})`);
	}
	return found;
}

function sleep(ms: number): Promise<null> {
	const { promise, resolve } = Promise.withResolvers<null>();
	setTimeout(() => resolve(null), ms);
	return promise;
}

async function stopSession(session: Session): Promise<number | null> {
	try {
		if (session.sock) {
			await request(session, { op: "quit" }, 2_000);
		} else if (session.exit === null) {
			// Non-omp-tui apps have no debug socket; Ctrl-C is the
			// conventional quit chord.
			session.proc.terminal.write("\x03");
		}
	} catch {
		// Fall through to signals.
	}
	const exited = await Promise.race([session.proc.exited, sleep(2_000)]);
	if (exited === null) {
		session.proc.kill("SIGKILL");
		await session.proc.exited.catch(() => {});
	}
	session.sock?.destroy();
	session.proc.terminal.close();
	rmSync(session.dir, { recursive: true, force: true });
	sessions.delete(session.name);
	return exited ?? -9;
}

// ─── Response narrowing ──────────────────────────────────────────────────────

/** The string rows of a `lines` response field; anything else is empty. */
function stringLines(value: unknown): string[] {
	if (!Array.isArray(value)) return [];
	return value.filter((line): line is string => typeof line === "string");
}

/** Reads one property of an unknown JSON value; `undefined` when absent. */
function field(value: unknown, name: string): unknown {
	if (value && typeof value === "object" && name in value) {
		return Reflect.get(value, name);
	}
	return undefined;
}

// ─── Rendering helpers ───────────────────────────────────────────────────────

function screenshotText(response: Record<string, unknown>): string {
	const lines = stringLines(response.lines);
	const header =
		`── viewport (window_top=${response.window_top}` +
		`${response.alt_screen ? ", alt screen" : ""}` +
		`${response.cursor ? `, cursor=${JSON.stringify(response.cursor)}` : ""}) ──`;
	return `${header}\n${lines.map((line) => `│${line}`).join("\n")}`;
}

/**
 * After-input screenshot: the renderer's viewport when a debug socket exists
 * (and answers), else the emulator's screen.
 */
async function settled(session: Session, note: string, waitMs = 180): Promise<string> {
	await sleep(waitMs);
	if (session.sock) {
		try {
			const shot = await request(session, { op: "text" }, 3_000);
			if (shot.ok !== false) return `${note}\n${screenshotText(shot)}`;
		} catch {
			// Renderer unavailable; fall back to the emulator.
		}
	}
	return `${note}\n${session.screen.snapshot()}`;
}

/** Renders one `tree` response node (and its children) as outline rows. */
function renderTree(node: unknown, depth: number, out: string[]) {
	if (!node || typeof node !== "object") return;
	const rect = field(node, "rect");
	const id = field(node, "id");
	const flags = [
		field(node, "focused") === true ? "FOCUSED" : "",
		field(node, "focusable") === true ? "focusable" : "",
		field(node, "hidden") === true ? "hidden" : "",
	]
		.filter(Boolean)
		.join(" ");
	out.push(
		"  ".repeat(depth) +
			`${field(node, "kind")}${typeof id === "string" ? `#${id}` : ""}` +
			(Array.isArray(rect) ? ` [${rect[0]},${rect[1]} ${rect[2]}x${rect[3]}]` : "") +
			(flags ? `  ${flags}` : ""),
	);
	const children = field(node, "children");
	if (Array.isArray(children)) {
		for (const child of children) renderTree(child, depth + 1, out);
	}
}

const SEQUENCES: Record<string, string> = {
	alt_enter: "\x1b[?1049h",
	alt_leave: "\x1b[?1049l",
	clear_scrollback: "\x1b[3J",
	sync_begin: "\x1b[?2026h",
	sync_end: "\x1b[?2026l",
	mouse_on: "\x1b[?1003h",
	mouse_off: "\x1b[?1003l",
	cursor_hide: "\x1b[?25l",
	cursor_show: "\x1b[?25h",
};

function count(haystack: Buffer, needle: string): number {
	const bytes = Buffer.from(needle, "latin1");
	let total = 0;
	let from = 0;
	for (;;) {
		const index = haystack.indexOf(bytes, from);
		if (index < 0) return total;
		total += 1;
		from = index + bytes.length;
	}
}

/** Escapes control bytes so raw terminal output is printable. */
function visible(bytes: Buffer): string {
	let out = "";
	for (const byte of bytes) {
		if (byte === 0x1b) out += "\\e";
		else if (byte === 0x0a) out += "\n";
		else if (byte === 0x0d) out += "\\r";
		else if (byte < 0x20 || byte === 0x7f)
			out += `\\x${byte.toString(16).padStart(2, "0")}`;
		else out += String.fromCharCode(byte);
	}
	return out;
}

/** Unescapes `\e`, `\r`, `\n`, `\t`, and `\xNN` in a `send` payload. */
function unescapeBytes(text: string): Buffer {
	const out: number[] = [];
	for (let index = 0; index < text.length; index++) {
		if (text[index] !== "\\") {
			out.push(...Buffer.from(text[index], "utf8"));
			continue;
		}
		const next = text[index + 1];
		if (next === "e") {
			out.push(0x1b);
			index++;
		} else if (next === "r") {
			out.push(0x0d);
			index++;
		} else if (next === "n") {
			out.push(0x0a);
			index++;
		} else if (next === "t") {
			out.push(0x09);
			index++;
		} else if (next === "x") {
			out.push(Number.parseInt(text.slice(index + 2, index + 4), 16));
			index += 3;
		} else {
			out.push(0x5c);
		}
	}
	return Buffer.from(out);
}

// ─── Tool ────────────────────────────────────────────────────────────────────

const factory = (omp: ToolHost) => {
	const startSession = async (
		params: TuiParams,
		onUpdate?: (update: ToolUpdate) => void,
	): Promise<string> => {
		const name = params.name ?? "main";
		if (sessions.has(name)) {
			throw new Error(`session "${name}" already running; stop it first`);
		}
		if (!params.example && !params.bin) {
			throw new Error("start needs `example` or `bin`");
		}
		const target = params.example ?? params.bin ?? "";
		if (params.build !== false) {
			onUpdate?.({ content: [{ type: "text", text: `building ${target}…` }] });
			const kind = params.example ? "--example" : "--bin";
			const built = await omp.exec("cargo", ["build", kind, target], { cwd: omp.cwd });
			if (built.code !== 0) {
				throw new Error(`cargo build failed:\n${built.stderr.slice(-4000)}`);
			}
		}
		const binary = params.example
			? join(omp.cwd, "target", "debug", "examples", target)
			: join(omp.cwd, "target", "debug", target);

		const rows = params.rows ?? 30;
		const cols = params.cols ?? 100;
		const dir = mkdtempSync(join(tmpdir(), `omp-tui-${name}-`));
		const sockPath = join(dir, "debug.sock");
		// The PTY data callback closes over `session`; Bun.spawn returns
		// synchronously and the callback fires on the event loop, so the
		// binding is assigned before the first chunk can arrive.
		let session: Session;
		const proc: Child = Bun.spawn([binary, ...(params.args ?? [])], {
			cwd: omp.cwd,
			env: {
				...process.env,
				OMP_TUI_DEBUG: sockPath,
				TERM: "xterm-256color",
				COLORTERM: "truecolor",
			},
			terminal: {
				cols,
				rows,
				data(_terminal: TerminalHandle, chunk: Buffer) {
					capture(session, chunk);
				},
			},
		});
		session = {
			name,
			target,
			proc,
			cols,
			rows,
			dir,
			screen: new Screen(cols, rows),
			sock: null,
			sockBuf: "",
			waiters: [],
			raw: [],
			rawBytes: 0,
			exit: null,
		};
		proc.exited.then((code) => {
			session.exit = code;
		});
		sessions.set(name, session);

		const deadline = Date.now() + (params.timeout ?? 15) * 1000;
		const connected = await connectSocket(
			session,
			sockPath,
			(params.timeout ?? 15) * 1000,
		);
		if (session.exit !== null) {
			const tail = visible(Buffer.concat(session.raw)).slice(-3000);
			await stopSession(session).catch(() => {});
			throw new Error(
				`"${target}" exited immediately (code ${session.exit}).\n` +
					`terminal tail: ${tail || "(empty)"}`,
			);
		}
		let text = `session "${name}": ${target} pid=${proc.pid} pty=${cols}x${rows}`;
		if (connected) {
			// The socket binds at terminal entry, before the first frame
			// paints; retry until the snapshot exists so `start` reliably
			// returns the opening screenshot.
			let shot = await request(session, { op: "text" });
			while (
				shot.ok === false &&
				String(shot.error ?? "").includes("no frame painted yet") &&
				session.exit === null &&
				Date.now() < deadline
			) {
				await sleep(50);
				shot = await request(session, { op: "text" });
			}
			text += `\n${screenshotText(shot)}`;
		} else {
			text +=
				"\n(no debug socket: app is not an omp-tui host; `send` injects " +
				"input, `screen` renders the emulated display)" +
				`\n${session.screen.snapshot()}`;
		}
		return text;
	};

	return {
		name: "tui",
		label: "TUI Debug",
		description:
			"Run and debug omp-tui apps (cargo examples or bins) headlessly on a real " +
			"PTY plus the OMP_TUI_DEBUG socket. Ops: start (example|bin, rows/cols, " +
			"args, build), text (viewport screenshot as plain text), screen " +
			"(plain-text screen from the built-in VT emulator — works for any app, no " +
			"debug socket needed; peek=N prepends N scrollback lines), frame (full " +
			"document), tree (component tree with ids/rects/focus), values (widget " +
			"values JSON), info, keys (spec like \"tab C-c enter 'literal'\"), type " +
			"(literal text through the input decoder), paste (bracketed paste), mouse " +
			"(x,y,action: click|right-click|middle-click|move|drag|release|wheel-up|" +
			"wheel-down), send (raw bytes to the terminal, \\e/\\xNN escapes), resize " +
			"(cols,rows delivered via SIGWINCH), raw (exact captured byte stream: " +
			"escape-sequence stats + escaped tail — prefer text/screen unless " +
			"auditing escapes), shot (pixel screenshot of the emulated screen — real " +
			"colors/styles as a PNG image, rasterized in-process), stop, list. " +
			"Input ops (keys/type/paste/mouse/send/" +
			"resize) return an after-screenshot of the resulting display (quiet:true " +
			"skips it). Sessions persist across calls; injected input rides the " +
			"app's real input path.",
		parameters: omp.zod.object({
			op: omp.zod
				.string()
				.describe(
					"operation: start | stop | list | text | screen | shot | frame | tree | values | info | keys | type | paste | mouse | send | resize | raw",
				),
			name: omp.zod.string().optional().describe("session name (default: main)"),
			example: omp.zod.string().optional().describe("start: cargo example name"),
			bin: omp.zod.string().optional().describe("start: cargo bin name"),
			args: omp.zod.array(omp.zod.string()).optional().describe("start: program argv"),
			rows: omp.zod.number().optional().describe("start/resize: pty rows (default 30)"),
			cols: omp.zod.number().optional().describe("start/resize: pty cols (default 100)"),
			build: omp.zod
				.boolean()
				.optional()
				.describe("start: cargo build first (default true)"),
			keys: omp.zod
				.string()
				.optional()
				.describe("keys: spec, e.g. \"tab tab enter C-c pgdn 'hello'\""),
			text: omp.zod.string().optional().describe("type/paste/send: payload text"),
			x: omp.zod.number().optional().describe("mouse: zero-based column"),
			y: omp.zod.number().optional().describe("mouse: zero-based viewport row"),
			action: omp.zod.string().optional().describe("mouse: gesture (default click)"),
			peek: omp.zod
				.number()
				.optional()
				.describe("screen: scrollback lines to include; raw: tail bytes (default 2000)"),
			clear: omp.zod.boolean().optional().describe("raw: reset capture after reading"),
			quiet: omp.zod
				.boolean()
				.optional()
				.describe("input ops: skip the after-screenshot"),
			timeout: omp.zod
				.number()
				.optional()
				.describe("start: socket wait seconds (default 15)"),
		}),

		async execute(
			_toolCallId: string,
			params: TuiParams,
			onUpdate?: (update: ToolUpdate) => void,
		): Promise<ToolResult> {
			const reply = (text: string, details?: Record<string, unknown>): ToolResult => ({
				content: [{ type: "text", text }],
				details,
			});

			switch (params.op) {
				case "start":
					return reply(await startSession(params, onUpdate));
				case "list": {
					const rows = [...sessions.values()].map(
						(session) =>
							`${session.name}: ${session.target} pid=${session.proc.pid} ` +
							`${session.cols}x${session.rows} ` +
							`${session.exit === null ? "running" : `exited(${session.exit})`}` +
							`${session.sock ? "" : " (no socket)"}`,
					);
					return reply(rows.join("\n") || "no sessions");
				}
				case "stop": {
					const session = need(params.name);
					const code = await stopSession(session);
					return reply(`stopped "${session.name}" (exit ${code})`);
				}
				case "text": {
					const session = need(params.name);
					if (!session.sock) return reply(session.screen.snapshot(params.peek ?? 0));
					const response = await request(session, { op: "text" });
					return reply(screenshotText(response), response);
				}
				case "screen": {
					return reply(need(params.name).screen.snapshot(params.peek ?? 0));
				}
				case "shot": {
					const session = need(params.name);
					const png = join(tmpdir(), `omp-tui-${session.name}-${Date.now()}.png`);
					const bytes = await session.screen.png();
					writeFileSync(png, bytes);
					return {
						content: [
							{ type: "image", data: bytes.toString("base64"), mimeType: "image/png" },
							{ type: "text", text: `screenshot saved: ${png}` },
						],
					};
				}
				case "frame": {
					const response = await request(need(params.name), { op: "frame" });
					const lines = stringLines(response.lines);
					return reply(lines.map((line) => `│${line}`).join("\n"), response);
				}
				case "tree": {
					const response = await request(need(params.name), { op: "tree" });
					const out: string[] = [];
					renderTree(field(response.tree, "root"), 0, out);
					const overlays = field(response.tree, "overlays");
					if (Array.isArray(overlays)) {
						for (const layer of overlays) {
							out.push(
								`overlay #${field(layer, "overlay")} ` +
									`band=${JSON.stringify(field(layer, "band"))}` +
									`${field(layer, "hidden") === true ? " hidden" : ""}`,
							);
							renderTree(field(layer, "root"), 1, out);
						}
					}
					return reply(out.join("\n"), response);
				}
				case "values":
				case "info": {
					const response = await request(need(params.name), { op: params.op });
					return reply(JSON.stringify(response, null, 1), response);
				}
				case "keys": {
					if (!params.keys) throw new Error("keys op needs `keys`");
					const session = need(params.name);
					const response = await request(session, { op: "keys", keys: params.keys });
					if (!response.ok) throw new Error(String(response.error));
					const note = `injected ${response.injected} events`;
					return reply(params.quiet ? note : await settled(session, note), response);
				}
				case "type": {
					if (params.text === undefined) throw new Error("type op needs `text`");
					const session = need(params.name);
					const response = await request(session, { op: "bytes", data: params.text });
					if (!response.ok) throw new Error(String(response.error));
					const note = `typed ${params.text.length} chars`;
					return reply(params.quiet ? note : await settled(session, note), response);
				}
				case "paste": {
					if (params.text === undefined) throw new Error("paste op needs `text`");
					const session = need(params.name);
					const response = await request(session, { op: "paste", text: params.text });
					if (!response.ok) throw new Error(String(response.error));
					return reply(params.quiet ? "pasted" : await settled(session, "pasted"), response);
				}
				case "mouse": {
					if (params.x === undefined || params.y === undefined) {
						throw new Error("mouse op needs `x` and `y`");
					}
					const session = need(params.name);
					const response = await request(session, {
						op: "mouse",
						x: params.x,
						y: params.y,
						action: params.action ?? "click",
					});
					if (!response.ok) throw new Error(String(response.error));
					const note = `mouse ${params.action ?? "click"} at ${params.x},${params.y}`;
					return reply(params.quiet ? note : await settled(session, note), response);
				}
				case "send": {
					if (params.text === undefined) throw new Error("send op needs `text`");
					const session = need(params.name);
					const bytes = unescapeBytes(params.text);
					session.proc.terminal.write(bytes);
					const note = `sent ${bytes.length} bytes to the terminal`;
					return reply(params.quiet ? note : await settled(session, note));
				}
				case "resize": {
					const session = need(params.name);
					const rows = params.rows ?? session.rows;
					const cols = params.cols ?? session.cols;
					session.proc.terminal.resize(cols, rows);
					session.screen.resize(cols, rows);
					session.cols = cols;
					session.rows = rows;
					const note = `resized to ${cols}x${rows}; SIGWINCH delivered`;
					return reply(params.quiet ? note : await settled(session, note, 350));
				}
				case "raw": {
					const session = need(params.name);
					const blob = Buffer.concat(session.raw);
					const stats: Record<string, number> = { bytes: blob.length };
					for (const key in SEQUENCES) {
						const total = count(blob, SEQUENCES[key]);
						if (total > 0) stats[key] = total;
					}
					if (params.clear) {
						session.raw = [];
						session.rawBytes = 0;
					}
					const peek = params.peek ?? 2000;
					const tail = peek > 0 ? visible(blob.subarray(-peek)) : "";
					return reply(
						`${JSON.stringify(stats)}${tail ? `\n── tail ──\n${tail}` : ""}`,
						{ stats },
					);
				}
				default:
					throw new Error(`unknown op ${JSON.stringify(params.op)}`);
			}
		},

		onSession(event: { reason?: string }) {
			if (event.reason === "shutdown") {
				for (const session of sessions.values()) {
					try {
						session.proc.kill("SIGKILL");
						session.proc.terminal.close();
						rmSync(session.dir, { recursive: true, force: true });
					} catch {
						// Best-effort teardown.
					}
				}
				sessions.clear();
			}
		},
	};
};

export default factory;
