// Checks the UI catalogue without a browser:
// - every language has exactly the same keys and {placeholders};
// - every key used by index.html and app.js exists;
// - every error code Rust can return has a translation;
// - the READMEs' troubleshooting tables list only real codes, the same
//   ones in every language.
// Run from the repository root: node scripts/check-ui.mjs
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import vm from "node:vm";

const failures = [];
const fail = (message) => failures.push(message);

const context = vm.createContext({
  window: { localStorage: { getItem: () => null, setItem: () => {} } },
  navigator: { languages: ["en"] },
  document: {
    documentElement: {},
    querySelectorAll: () => [],
    dispatchEvent: () => {},
  },
  CustomEvent: class {},
});
const catalog = vm.runInContext(`${readFileSync("ui/i18n.js", "utf8")}\n;I18N.catalog`, context);

const languages = Object.keys(catalog);
const placeholders = (text) => [...text.matchAll(/\{(\w+)\}/g)].map((match) => match[1]).sort().join(",");
const english = catalog.en;
for (const language of languages) {
  const keys = new Set(Object.keys(catalog[language]));
  for (const key of Object.keys(english)) {
    if (!keys.has(key)) {
      fail(`${language}: missing ${key}`);
    } else if (placeholders(catalog[language][key]) !== placeholders(english[key])) {
      fail(`${language}: placeholders differ for ${key}`);
    } else if (!catalog[language][key].trim()) {
      fail(`${language}: empty ${key}`);
    }
  }
  for (const key of keys) {
    if (!(key in english)) {
      fail(`${language}: extra key ${key}`);
    }
  }
}

const html = readFileSync("ui/index.html", "utf8");
for (const match of html.matchAll(/data-i18n(?:-aria-label)?="([^"]+)"/g)) {
  if (!(match[1] in english)) {
    fail(`index.html uses unknown key ${match[1]}`);
  }
}
const app = readFileSync("ui/app.js", "utf8");
const keyPattern = /"((?:progress|cancel|cancelled|download|success|deps|error|network|hint|diagnostics|add|queue|job)\.[A-Za-z_.]+)"/g;
for (const match of app.matchAll(keyPattern)) {
  if (!(match[1] in english)) {
    fail(`app.js uses unknown key ${match[1]}`);
  }
}

// Error codes returned by Rust. A few are handled without a message key.
const handledElsewhere = new Set(["cancelled"]);
const variants = { access_denied: true, host_firewall_block: true };
const rustFiles = [];
const walk = (directory) => {
  for (const name of readdirSync(directory)) {
    const path = join(directory, name);
    if (statSync(path).isDirectory()) {
      walk(path);
    } else if (name.endsWith(".rs") && name !== "e2e.rs") {
      rustFiles.push(path);
    }
  }
};
walk("crates");
walk("src-tauri/src");
const codes = new Set();
for (const file of rustFiles) {
  const source = readFileSync(file, "utf8").split("#[cfg(test)]\nmod tests")[0];
  for (const match of source.matchAll(/PublicError::new\(\s*"([a-z_]+)"/g)) {
    codes.add(match[1]);
  }
  for (const match of source.matchAll(/"(access_denied|host_firewall_block)"/g)) {
    codes.add(match[1]);
  }
}
for (const code of codes) {
  if (handledElsewhere.has(code)) {
    continue;
  }
  const keys = variants[code]
    ? ["page", "bare", "direct"].map((kind) => `error.${code}.${kind}`)
    : [`error.${code}`];
  for (const key of keys) {
    if (!(key in english)) {
      fail(`Rust error code ${code} has no translation ${key}`);
    }
  }
}
if (codes.size < 10) {
  fail(`found only ${codes.size} Rust error codes; the scan is broken`);
}

// The READMEs' troubleshooting tables: every code listed must exist in
// Rust, and the three languages must list the same codes.
const readmeCodes = {};
for (const readme of ["README.md", "README.vi.md", "README.ja.md"]) {
  const listed = new Set();
  for (const row of readFileSync(readme, "utf8").matchAll(/^\| (`[a-z_]+`(?: \/ `[a-z_]+`)*) \|/gm)) {
    for (const code of row[1].matchAll(/`([a-z_]+)`/g)) {
      listed.add(code[1]);
      if (!codes.has(code[1])) {
        fail(`${readme} documents ${code[1]}, which Rust no longer returns`);
      }
    }
  }
  readmeCodes[readme] = [...listed].sort().join(" ");
}
if (new Set(Object.values(readmeCodes)).size !== 1) {
  fail(`README troubleshooting tables differ: ${JSON.stringify(readmeCodes)}`);
}

if (failures.length) {
  console.error(failures.join("\n"));
  process.exit(1);
}
console.log(
  `ui check ok: ${languages.join("/")} × ${Object.keys(english).length} keys, ${codes.size} Rust error codes, ${readmeCodes["README.md"].split(" ").length} documented`,
);
