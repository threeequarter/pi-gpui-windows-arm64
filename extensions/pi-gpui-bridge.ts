/**
 * Pi GPUI bridge.
 *
 * Pi GPUI loads this file with `pi --mode rpc --extension <path>` for its
 * own pi processes only; nothing is installed. It fills two gaps in pi's RPC
 * protocol:
 *
 * 1. Moving within the session tree: the app sends
 *    `/pi-gpui-navigate <entryId> [summarize]` as a prompt and reloads the
 *    transcript when pi answers "handled".
 * 2. Usage details for the context popover: the last turn's tokens, the
 *    prompt cache lifetime, whether the provider bills a subscription, and
 *    plan limits from provider response headers. They are published as JSON
 *    through `setStatus("pi-gpui:usage", ...)`, which RPC forwards.
 * 3. Component widgets: RPC forwards `setWidget` only for string arrays and
 *    drops component factories. The bridge hosts those components itself,
 *    renders them to ANSI-styled lines and forwards the lines.
 */
const STATUS_KEY = "pi-gpui:usage";
const WIDGET_COLUMNS = 160;
const WIDGET_ROWS = 50;
const WIDGET_RENDER_DELAY_MS = 50;
// chalk is colorless on RPC's piped stdout, so the theme's text styles emit SGR directly.
const SGR_STYLES: Record<string, readonly [number, number]> = {
	bold: [1, 22],
	italic: [3, 23],
	underline: [4, 24],
	inverse: [7, 27],
	strikethrough: [9, 29],
};

const hostedUIs = new WeakSet<object>();

type PlanLimit = { windowMinutes: number; usedPercent: number; resetsAt?: number };

export default function (pi: any) {
	let limits: { provider: string; limits: PlanLimit[] } | undefined;

	pi.registerCommand("pi-gpui-navigate", {
		description: "Internal: used by Pi GPUI to move within the session tree",
		handler: async (args: string, ctx: any) => {
			const [targetId, mode] = args.trim().split(/\s+/);
			if (!targetId) return;
			await ctx.waitForIdle?.();
			await ctx.navigateTree(targetId, { summarize: mode === "summarize" });
		},
	});

	const publish = (ctx: any) => {
		try {
			const model = ctx.model;
			if (!model || !ctx.hasUI) return;
			const branch = ctx.sessionManager.getBranch();
			const last = latestAssistantUsage(branch);
			let touchedAt: number | undefined;
			if (last && last.provider === model.provider && last.model === model.id) {
				touchedAt = last.at;
				for (const entry of branch) {
					if (entry.type !== "usage" || entry.kind !== "cache_warm" || entry.provider !== model.provider) continue;
					const at = Date.parse(entry.timestamp);
					if (Number.isFinite(at) && at > touchedAt) touchedAt = at;
				}
			}
			const retention = process.env.PI_CACHE_RETENTION === "long" ? "long" : "short";
			let subscription = model.provider === "kimi-coding";
			try {
				subscription ||= Boolean(ctx.modelRegistry?.isUsingOAuth?.(model));
			} catch {}
			ctx.ui.setStatus(
				STATUS_KEY,
				JSON.stringify({
					subscription,
					lastTurn: last?.counts,
					cacheSeconds: model.promptCache?.[retention],
					cacheTouchedAt: touchedAt,
					limits: limits && limits.provider === model.provider ? limits.limits : undefined,
				}),
			);
		} catch {}
	};

	pi.on("session_start", (_event: unknown, ctx: any) => {
		if (ctx.hasUI) hostComponentWidgets(ctx.ui);
		publish(ctx);
	});
	pi.on("model_select", (_event: unknown, ctx: any) => publish(ctx));
	pi.on("agent_settled", (_event: unknown, ctx: any) => publish(ctx));
	pi.on("after_provider_response", (event: any, ctx: any) => {
		const provider = ctx.model?.provider;
		if (!provider || event.status < 200 || event.status >= 300) return;
		const parsed = parsePlanLimits(event.headers ?? {});
		if (parsed.length === 0) return;
		limits = { provider, limits: parsed };
		publish(ctx);
	});
}

/**
 * Replaces `setWidget` on the session's UI context, which every extension
 * shares, so component factories render here instead of being dropped.
 * pi loads `--extension` files first, so this runs before other extensions'
 * `session_start` handlers.
 */
function hostComponentWidgets(ui: any) {
	if (hostedUIs.has(ui)) return;
	hostedUIs.add(ui);
	const forward = ui.setWidget;
	const theme = styledTheme(ui.theme);
	ui.theme = theme;
	const hosts = new Map<string, { dispose: () => void }>();
	ui.setWidget = (key: string, content: unknown, options?: { placement?: string }) => {
		hosts.get(key)?.dispose();
		hosts.delete(key);
		if (typeof content !== "function") {
			forward(key, content, options);
			return;
		}
		try {
			hosts.set(key, hostWidget(content, theme, (lines) => forward(key, lines, options)));
		} catch (error) {
			console.error(`pi-gpui: widget ${key} failed to start: ${error}`);
			forward(key, undefined, options);
		}
	};
}

function hostWidget(factory: (tui: unknown, theme: unknown) => any, theme: unknown, publish: (lines: string[]) => void) {
	let timer: ReturnType<typeof setTimeout> | undefined;
	let disposed = false;
	let published = "";
	const render = () => {
		timer = undefined;
		if (disposed) return;
		let lines: string[] = [];
		try {
			lines = component.render(WIDGET_COLUMNS);
		} catch (error) {
			console.error(`pi-gpui: widget render failed: ${error}`);
		}
		const serialized = JSON.stringify(lines);
		if (serialized === published) return;
		published = serialized;
		publish(lines);
	};
	const requestRender = () => {
		if (!disposed && !timer) timer = setTimeout(render, WIDGET_RENDER_DELAY_MS);
	};
	const tui = { requestRender, terminal: { columns: WIDGET_COLUMNS, rows: WIDGET_ROWS } };
	const component = factory(tui, theme);
	requestRender();
	return {
		dispose: () => {
			disposed = true;
			clearTimeout(timer);
			component.dispose?.();
		},
	};
}

function styledTheme(theme: any) {
	return new Proxy(theme, {
		get(target, property) {
			const codes = typeof property === "string" ? SGR_STYLES[property] : undefined;
			if (codes) return (text: string) => `\x1b[${codes[0]}m${text}\x1b[${codes[1]}m`;
			const value = Reflect.get(target, property);
			return typeof value === "function" ? value.bind(target) : value;
		},
	});
}

function latestAssistantUsage(branch: any[]) {
	for (let index = branch.length - 1; index >= 0; index -= 1) {
		const entry = branch[index];
		if (entry?.type !== "message" || entry.message?.role !== "assistant") continue;
		const message = entry.message;
		if (message.stopReason === "aborted" || message.stopReason === "error") continue;
		const { input = 0, output = 0, cacheRead = 0, cacheWrite = 0 } = message.usage ?? {};
		if (input + output + cacheRead + cacheWrite === 0) continue;
		return {
			counts: { input, output, cacheRead, cacheWrite },
			provider: message.provider,
			model: message.model,
			at: message.timestamp,
		};
	}
	return undefined;
}

/** Subscription limits: ChatGPT (Codex) and Claude unified rate-limit headers. */
function parsePlanLimits(raw: Record<string, string>): PlanLimit[] {
	const headers = new Map(Object.entries(raw).map(([name, value]) => [name.toLowerCase(), value]));
	const number = (name: string) => {
		const value = headers.get(name);
		if (value === undefined || value.trim() === "") return undefined;
		const parsed = Number(value);
		return Number.isFinite(parsed) ? parsed : undefined;
	};
	const limits: PlanLimit[] = [];
	for (const slot of ["primary", "secondary"]) {
		const windowMinutes = number(`x-codex-${slot}-window-minutes`);
		const usedPercent = number(`x-codex-${slot}-used-percent`);
		if (!windowMinutes || usedPercent === undefined) continue;
		const resetAt = number(`x-codex-${slot}-reset-at`);
		const resetAfter = number(`x-codex-${slot}-reset-after-seconds`);
		const resetsAt = resetAt ? resetAt * 1000 : resetAfter ? Date.now() + resetAfter * 1000 : undefined;
		limits.push({ windowMinutes, usedPercent, resetsAt });
	}
	for (const [slot, windowMinutes] of [["5h", 300], ["7d", 10080]] as const) {
		const utilization = number(`anthropic-ratelimit-unified-${slot}-utilization`);
		if (utilization === undefined) continue;
		const reset = number(`anthropic-ratelimit-unified-${slot}-reset`);
		limits.push({ windowMinutes, usedPercent: utilization * 100, resetsAt: reset ? reset * 1000 : undefined });
	}
	return limits;
}
