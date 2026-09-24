import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { getBundledSkillsDir } from "../src/config.js";
import { DefaultPackageManager } from "../src/core/package-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { loadSkillsFromDir } from "../src/core/skills.js";

function writeSkill(dir: string, name: string, description = `Description for ${name}`): void {
	const skillDir = join(dir, name);
	mkdirSync(skillDir, { recursive: true });
	writeFileSync(
		join(skillDir, "SKILL.md"),
		`---
name: ${name}
description: ${description}
---
Content for ${name}.`,
	);
}

describe("builtin skills", () => {
	let tempDir: string;
	let agentDir: string;
	let cwd: string;
	let bundledDir: string;
	let settingsManager: SettingsManager;

	beforeEach(() => {
		tempDir = join(tmpdir(), `builtin-skills-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		agentDir = join(tempDir, "agent");
		cwd = join(tempDir, "project");
		bundledDir = join(tempDir, "bundled-skills");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(cwd, { recursive: true });
		mkdirSync(bundledDir, { recursive: true });
		settingsManager = SettingsManager.inMemory();
	});

	afterEach(() => {
		rmSync(tempDir, { recursive: true, force: true });
	});

	const resolveWith = (bundledSkillsDir: string) =>
		new DefaultPackageManager({ cwd, agentDir, settingsManager, bundledSkillsDir }).resolve();

	it("resolves bundled skills as builtin and ranks them below same-named user skills", async () => {
		writeSkill(bundledDir, "shared-skill");
		writeSkill(join(agentDir, "skills"), "shared-skill");

		const result = await resolveWith(bundledDir);

		const builtinPath = join(bundledDir, "shared-skill", "SKILL.md");
		const builtin = result.skills.find((r) => r.path === builtinPath);
		expect(builtin?.enabled).toBe(true);
		expect(builtin?.metadata).toMatchObject({ source: "builtin", scope: "user", baseDir: bundledDir });

		const paths = result.skills.map((r) => r.path);
		expect(paths.indexOf(join(agentDir, "skills", "shared-skill", "SKILL.md"))).toBeLessThan(
			paths.indexOf(builtinPath),
		);
	});

	// A missing bundled dir is a release packaging slip, so it must surface as a diagnostic.
	it.each<[string, boolean, boolean]>([
		["missing dir warns", false, true],
		["populated dir stays quiet", true, false],
	])("bundled skills diagnostic: %s", async (_label, populated, expectWarning) => {
		const dir = populated ? bundledDir : join(tempDir, "does-not-exist");
		if (populated) writeSkill(dir, "builtin-skill");

		const result = await resolveWith(dir);

		const warning = result.diagnostics.find((d) => d.path === dir && d.type === "warning");
		expect(Boolean(warning)).toBe(expectWarning);
	});

	it("drops bundled skills entirely when built-in skills are disabled", async () => {
		writeSkill(bundledDir, "builtin-skill");
		settingsManager.setEnableBuiltinSkills(false);

		const result = await resolveWith(bundledDir);

		expect(result.skills.some((r) => r.metadata.source === "builtin")).toBe(false);
		expect(result.diagnostics.some((d) => d.path === bundledDir)).toBe(false);
	});

	it("loads every shipped skill without diagnostics", () => {
		const { skills, diagnostics } = loadSkillsFromDir({ dir: getBundledSkillsDir(), source: "builtin" });

		expect(diagnostics).toEqual([]);
		expect(skills.length).toBeGreaterThan(0);
		const names = skills.map((s) => s.name);
		expect(names).toEqual(expect.arrayContaining(["prime-intellect", "skill-creator"]));
		expect(names).not.toContain("orchestration-heartbeat");
		// Python skills must keep their import name: the kernel pre-imports by it.
		const pythonImports = new Map(
			skills.flatMap((s) => (s.kind === "python" ? [[s.name, s.python.importName] as const] : [])),
		);
		for (const [name, importName] of [
			["goal", "goal"],
			["compact", "compact"],
			["rlm-heartbeat", "rlm_heartbeat"],
			["edit", "edit"],
			// Fork-bundled python skills.
			["agent-sessions", "agent_sessions"],
			["aside", "aside"],
		]) {
			expect(pythonImports.get(name)).toBe(importName);
		}
	});

	it("loads the skill-creator python template as a valid python skill", () => {
		const referencePath = join(getBundledSkillsDir(), "skill-creator", "references", "python-skills.md");
		const reference = readFileSync(referencePath, "utf-8");
		const section = reference.match(/## Minimal Template([\s\S]*?)\n## /)?.[1];
		expect(section).toBeDefined();

		const files = [...(section as string).matchAll(/\*\*`([^`]+)`\*\*\s*\n+```[a-z]*\n([\s\S]*?)\n```/g)];
		expect(files.map((m) => m[1])).toEqual(["SKILL.md", "pyproject.toml", "src/word_count/__init__.py"]);

		const templateRoot = join(tempDir, "template-skills");
		for (const [, relPath, content] of files) {
			const filePath = join(templateRoot, "word-count", relPath);
			mkdirSync(dirname(filePath), { recursive: true });
			writeFileSync(filePath, content);
		}

		const { skills, diagnostics } = loadSkillsFromDir({ dir: templateRoot, source: "test" });
		expect(diagnostics).toEqual([]);
		expect(skills).toHaveLength(1);
		expect(skills[0].name).toBe("word-count");
		expect(skills[0].kind).toBe("python");
		expect(skills[0].kind === "python" && skills[0].python.importName).toBe("word_count");
	});

	// Verify every shipping path includes bundled skills; source-only success would hide a release packaging regression.
	describe("packaging ships bundled skills", () => {
		const packageRoot = join(__dirname, "..");
		const repoRoot = join(packageRoot, "..", "..");

		it("npm build (copy-assets) copies skills into dist", () => {
			const pkg = JSON.parse(readFileSync(join(packageRoot, "package.json"), "utf-8")) as {
				files?: string[];
				scripts?: Record<string, string>;
			};
			expect(pkg.scripts?.["copy-assets"]).toContain("skills dist/skills");
			// npm publish ships the source skills/ dir via the files allowlist too.
			expect(pkg.files).toContain("skills");
		});

		it("binary release script copies skills next to the executable", () => {
			const script = readFileSync(join(repoRoot, "scripts", "build-binaries.sh"), "utf-8");
			expect(script).toMatch(/cp -r skills binaries\/\$platform\//);
		});

		it("release packer includes skills in the packed package", () => {
			const script = readFileSync(join(repoRoot, "scripts", "pack-prime-agent-release.mjs"), "utf-8");
			expect(script).toContain('"skills"');
		});
	});
});
