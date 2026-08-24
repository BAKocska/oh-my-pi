## What the pi original did

`@isr4el-silv4/pi-script-tools` scanned a scripts directory, parsed annotations from shell-script headers, and registered every discovered script as a model-callable tool. Its live registry changed as scripts appeared, changed, or disappeared.

## The omp shape

This port treats discovery as data. On `extension_activate`, it streams regular `.sh` entries below the configured `scripts_dir` through `omp.env.find.walk`, so discovery shares the workspace cache and `.gitignore`/`.ignore` semantics documented in `docs/py/11-env.md` §“Workspace search — `omp.env.find`”. A leading `# @describe …` supplies the catalog summary; each `# @arg <name> <string|integer|number|boolean> <help>` supplies one required JSON-schema property in declaration order.

The manifest and IMPORT phase declare only the `scripts` parent. ACTIVATE mounts runtime-discovered relative leaves with `DynamicDeviceParent.mount_many`; rescans remount current definitions and batch removed/returned paths through `omp.devices.set_availability`. That is the dynamic-mount design frozen in `crates/py/python/omp/devices.py:1-19,303-402`: identity/schema/body mounting is distinct from reachability, and a discovered leaf cannot escape its manifest-authorized parent. It follows `docs/py/01-devices.md` §“`omp.devices`”, §“Availability transitions”, and §“`pi-mcp-adapter` → native mounting”: no unregister/re-register loop, one mounted-set transition, and a byte-identical core tool array. Every leaf remains behind the `xd` shell builtin, so any number of scripts consumes zero schema slots.

Execution is Environment-owned (`docs/py/11-env.md` §“Exec — `omp.env.sh`”). Typed values are converted to ordered argv strings, carried as scoped environment values, installed as quoted shell positional parameters, removed from the child environment, and passed to the discovered executable with `"$@"`. No argument value is concatenated into or interpreted as shell source.

Document `EXTERNAL_*`, `COMMITTED`, and `WATCH_RESCANNED` events trigger a fresh workspace scan. Existing paths are remounted so changed headers and bodies take effect; missing paths become unavailable in one batch, and newly returned paths become available. The journal/catalog mounted set remains the only truth.

## Gaps

None.
