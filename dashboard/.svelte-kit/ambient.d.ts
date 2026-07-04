
// this file is generated — do not edit it


/// <reference types="@sveltejs/kit" />

/**
 * This module provides access to environment variables that are injected _statically_ into your bundle at build time and are limited to _private_ access.
 * 
 * |         | Runtime                                                                    | Build time                                                               |
 * | ------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
 * | Private | [`$env/dynamic/private`](https://svelte.dev/docs/kit/$env-dynamic-private) | [`$env/static/private`](https://svelte.dev/docs/kit/$env-static-private) |
 * | Public  | [`$env/dynamic/public`](https://svelte.dev/docs/kit/$env-dynamic-public)   | [`$env/static/public`](https://svelte.dev/docs/kit/$env-static-public)   |
 * 
 * Static environment variables are [loaded by Vite](https://vitejs.dev/guide/env-and-mode.html#env-files) from `.env` files and `process.env` at build time and then statically injected into your bundle at build time, enabling optimisations like dead code elimination.
 * 
 * **_Private_ access:**
 * 
 * - This module cannot be imported into client-side code
 * - This module only includes variables that _do not_ begin with [`config.kit.env.publicPrefix`](https://svelte.dev/docs/kit/configuration#env) _and do_ start with [`config.kit.env.privatePrefix`](https://svelte.dev/docs/kit/configuration#env) (if configured)
 * 
 * For example, given the following build time environment:
 * 
 * ```env
 * ENVIRONMENT=production
 * PUBLIC_BASE_URL=http://site.com
 * ```
 * 
 * With the default `publicPrefix` and `privatePrefix`:
 * 
 * ```ts
 * import { ENVIRONMENT, PUBLIC_BASE_URL } from '$env/static/private';
 * 
 * console.log(ENVIRONMENT); // => "production"
 * console.log(PUBLIC_BASE_URL); // => throws error during build
 * ```
 * 
 * The above values will be the same _even if_ different values for `ENVIRONMENT` or `PUBLIC_BASE_URL` are set at runtime, as they are statically replaced in your code with their build time values.
 */
declare module '$env/static/private' {
	export const SVELTEKIT_FORK: string;
	export const HERMES_HOME: string;
	export const OLDPWD: string;
	export const LC_NUMERIC: string;
	export const npm_node_execpath: string;
	export const HERMES_SESSION_KEY: string;
	export const BROWSERBASE_ADVANCED_STEALTH: string;
	export const CSF_TObjDefaults: string;
	export const npm_config_noproxy: string;
	export const HERMES_MAX_ITERATIONS: string;
	export const MMGT_CLEAR: string;
	export const CSF_XSMessage: string;
	export const TERMINAL_DOCKER_MOUNT_CWD_TO_WORKSPACE: string;
	export const HERMES_REDACT_SECRETS: string;
	export const PATH: string;
	export const TERMINAL_DOCKER_VOLUMES: string;
	export const npm_package_json: string;
	export const TERMINAL_CWD: string;
	export const TERMINAL_HOME_MODE: string;
	export const TERMINAL_PERSISTENT_SHELL: string;
	export const TERMINAL_DAYTONA_IMAGE: string;
	export const SHLVL: string;
	export const CSF_XCAFDefaults: string;
	export const USER: string;
	export const npm_config_prefix: string;
	export const LC_MEASUREMENT: string;
	export const npm_package_name: string;
	export const BROWSER_INACTIVITY_TIMEOUT: string;
	export const HERMES_WEBUI_PORT: string;
	export const npm_config_npm_version: string;
	export const VISION_TOOLS_DEBUG: string;
	export const CSF_XmlOcafResource: string;
	export const npm_config_user_agent: string;
	export const CSF_EXCEPTION_PROMPT: string;
	export const CSF_ShadersDirectory: string;
	export const CSF_PluginDefaults: string;
	export const BROWSERBASE_PROXIES: string;
	export const DEBUGINFOD_URLS: string;
	export const INIT_CWD: string;
	export const LC_MONETARY: string;
	export const TERMINAL_CONTAINER_MEMORY: string;
	export const npm_config_globalconfig: string;
	export const CSF_MIGRATION_TYPES: string;
	export const TERMINAL_ENV: string;
	export const npm_config_local_prefix: string;
	export const DRAWHOME: string;
	export const MEMORY_PRESSURE_WRITE: string;
	export const LC_TIME: string;
	export const CSF_OCCTResourcePath: string;
	export const HERMES_WEBUI_HOST: string;
	export const CSF_IGESDefaults: string;
	export const TERMINAL_CONTAINER_DISK: string;
	export const TRIBUNAL_BOT_NAME: string;
	export const CSF_MDTVTexturesDirectory: string;
	export const _: string;
	export const TERMINAL_LIFETIME_SECONDS: string;
	export const CSF_StandardLiteDefaults: string;
	export const TERMINAL_DOCKER_IMAGE: string;
	export const XDG_DATA_DIRS: string;
	export const CASROOT: string;
	export const TERMINAL_MODAL_MODE: string;
	export const npm_lifecycle_event: string;
	export const HERMES_WEBUI_PASSWORD: string;
	export const LC_NAME: string;
	export const HERMES_QUIET: string;
	export const HERMES_WEBUI_AGENT_DIR: string;
	export const VSSCRIPT_PATH: string;
	export const npm_lifecycle_script: string;
	export const HERMES_SESSION_CHAT_ID: string;
	export const TERMINAL_CONTAINER_PERSISTENT: string;
	export const CSF_TObjMessage: string;
	export const AUXILIARY_VISION_MODEL: string;
	export const MOA_TOOLS_DEBUG: string;
	export const LC_IDENTIFICATION: string;
	export const HERMES_CRON_TIMEOUT: string;
	export const TRIBUNAL_ROLE: string;
	export const npm_config_global_prefix: string;
	export const BROWSER_SESSION_TIMEOUT: string;
	export const npm_package_version: string;
	export const npm_config_cache: string;
	export const npm_config_node_gyp: string;
	export const SHELL: string;
	export const CSF_STEPDefaults: string;
	export const TERMINAL_DOCKER_FORWARD_ENV: string;
	export const DRAWDEFAULT: string;
	export const HERMES_EXEC_ASK: string;
	export const NODE_ENV: string;
	export const MATRIX_ENCRYPTION: string;
	export const CSF_DrawPluginDefaults: string;
	export const CSF_LANGUAGE: string;
	export const LANG: string;
	export const TERMINAL_DOCKER_EXTRA_ARGS: string;
	export const TERMINAL_CONTAINER_CPU: string;
	export const NODE: string;
	export const TERMINAL_SINGULARITY_IMAGE: string;
	export const HERMES_WEBUI_STATE_DIR: string;
	export const INVOCATION_ID: string;
	export const CSF_SHMessage: string;
	export const EDITOR: string;
	export const TERMINAL_TIMEOUT: string;
	export const HERMES_SESSION_ID: string;
	export const COLOR: string;
	export const TRIBUNAL_AGENT_ID: string;
	export const npm_command: string;
	export const LOGNAME: string;
	export const HERMES_REAL_HOME: string;
	export const MEMORY_PRESSURE_WATCH: string;
	export const npm_config_userconfig: string;
	export const IMAGE_TOOLS_DEBUG: string;
	export const npm_config_init_module: string;
	export const npm_execpath: string;
	export const TERMINAL_MODAL_IMAGE: string;
	export const WEB_TOOLS_DEBUG: string;
	export const SYSTEMD_EXEC_PID: string;
	export const AUXILIARY_VISION_PROVIDER: string;
	export const CSF_StandardDefaults: string;
	export const HOME: string;
	export const LC_TELEPHONE: string;
	export const LC_PAPER: string;
	export const PWD: string;
	export const HERMES_SESSION_PLATFORM: string;
	export const LC_ADDRESS: string;
	export const TERMINAL_DOCKER_RUN_AS_HOST_USER: string;
}

/**
 * This module provides access to environment variables that are injected _statically_ into your bundle at build time and are _publicly_ accessible.
 * 
 * |         | Runtime                                                                    | Build time                                                               |
 * | ------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
 * | Private | [`$env/dynamic/private`](https://svelte.dev/docs/kit/$env-dynamic-private) | [`$env/static/private`](https://svelte.dev/docs/kit/$env-static-private) |
 * | Public  | [`$env/dynamic/public`](https://svelte.dev/docs/kit/$env-dynamic-public)   | [`$env/static/public`](https://svelte.dev/docs/kit/$env-static-public)   |
 * 
 * Static environment variables are [loaded by Vite](https://vitejs.dev/guide/env-and-mode.html#env-files) from `.env` files and `process.env` at build time and then statically injected into your bundle at build time, enabling optimisations like dead code elimination.
 * 
 * **_Public_ access:**
 * 
 * - This module _can_ be imported into client-side code
 * - **Only** variables that begin with [`config.kit.env.publicPrefix`](https://svelte.dev/docs/kit/configuration#env) (which defaults to `PUBLIC_`) are included
 * 
 * For example, given the following build time environment:
 * 
 * ```env
 * ENVIRONMENT=production
 * PUBLIC_BASE_URL=http://site.com
 * ```
 * 
 * With the default `publicPrefix` and `privatePrefix`:
 * 
 * ```ts
 * import { ENVIRONMENT, PUBLIC_BASE_URL } from '$env/static/public';
 * 
 * console.log(ENVIRONMENT); // => throws error during build
 * console.log(PUBLIC_BASE_URL); // => "http://site.com"
 * ```
 * 
 * The above values will be the same _even if_ different values for `ENVIRONMENT` or `PUBLIC_BASE_URL` are set at runtime, as they are statically replaced in your code with their build time values.
 */
declare module '$env/static/public' {
	
}

/**
 * This module provides access to environment variables set _dynamically_ at runtime and that are limited to _private_ access.
 * 
 * |         | Runtime                                                                    | Build time                                                               |
 * | ------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
 * | Private | [`$env/dynamic/private`](https://svelte.dev/docs/kit/$env-dynamic-private) | [`$env/static/private`](https://svelte.dev/docs/kit/$env-static-private) |
 * | Public  | [`$env/dynamic/public`](https://svelte.dev/docs/kit/$env-dynamic-public)   | [`$env/static/public`](https://svelte.dev/docs/kit/$env-static-public)   |
 * 
 * Dynamic environment variables are defined by the platform you're running on. For example if you're using [`adapter-node`](https://github.com/sveltejs/kit/tree/main/packages/adapter-node) (or running [`vite preview`](https://svelte.dev/docs/kit/cli)), this is equivalent to `process.env`.
 * 
 * **_Private_ access:**
 * 
 * - This module cannot be imported into client-side code
 * - This module includes variables that _do not_ begin with [`config.kit.env.publicPrefix`](https://svelte.dev/docs/kit/configuration#env) _and do_ start with [`config.kit.env.privatePrefix`](https://svelte.dev/docs/kit/configuration#env) (if configured)
 * 
 * > [!NOTE] In `dev`, `$env/dynamic` includes environment variables from `.env`. In `prod`, this behavior will depend on your adapter.
 * 
 * > [!NOTE] To get correct types, environment variables referenced in your code should be declared (for example in an `.env` file), even if they don't have a value until the app is deployed:
 * >
 * > ```env
 * > MY_FEATURE_FLAG=
 * > ```
 * >
 * > You can override `.env` values from the command line like so:
 * >
 * > ```sh
 * > MY_FEATURE_FLAG="enabled" npm run dev
 * > ```
 * 
 * For example, given the following runtime environment:
 * 
 * ```env
 * ENVIRONMENT=production
 * PUBLIC_BASE_URL=http://site.com
 * ```
 * 
 * With the default `publicPrefix` and `privatePrefix`:
 * 
 * ```ts
 * import { env } from '$env/dynamic/private';
 * 
 * console.log(env.ENVIRONMENT); // => "production"
 * console.log(env.PUBLIC_BASE_URL); // => undefined
 * ```
 */
declare module '$env/dynamic/private' {
	export const env: {
		SVELTEKIT_FORK: string;
		HERMES_HOME: string;
		OLDPWD: string;
		LC_NUMERIC: string;
		npm_node_execpath: string;
		HERMES_SESSION_KEY: string;
		BROWSERBASE_ADVANCED_STEALTH: string;
		CSF_TObjDefaults: string;
		npm_config_noproxy: string;
		HERMES_MAX_ITERATIONS: string;
		MMGT_CLEAR: string;
		CSF_XSMessage: string;
		TERMINAL_DOCKER_MOUNT_CWD_TO_WORKSPACE: string;
		HERMES_REDACT_SECRETS: string;
		PATH: string;
		TERMINAL_DOCKER_VOLUMES: string;
		npm_package_json: string;
		TERMINAL_CWD: string;
		TERMINAL_HOME_MODE: string;
		TERMINAL_PERSISTENT_SHELL: string;
		TERMINAL_DAYTONA_IMAGE: string;
		SHLVL: string;
		CSF_XCAFDefaults: string;
		USER: string;
		npm_config_prefix: string;
		LC_MEASUREMENT: string;
		npm_package_name: string;
		BROWSER_INACTIVITY_TIMEOUT: string;
		HERMES_WEBUI_PORT: string;
		npm_config_npm_version: string;
		VISION_TOOLS_DEBUG: string;
		CSF_XmlOcafResource: string;
		npm_config_user_agent: string;
		CSF_EXCEPTION_PROMPT: string;
		CSF_ShadersDirectory: string;
		CSF_PluginDefaults: string;
		BROWSERBASE_PROXIES: string;
		DEBUGINFOD_URLS: string;
		INIT_CWD: string;
		LC_MONETARY: string;
		TERMINAL_CONTAINER_MEMORY: string;
		npm_config_globalconfig: string;
		CSF_MIGRATION_TYPES: string;
		TERMINAL_ENV: string;
		npm_config_local_prefix: string;
		DRAWHOME: string;
		MEMORY_PRESSURE_WRITE: string;
		LC_TIME: string;
		CSF_OCCTResourcePath: string;
		HERMES_WEBUI_HOST: string;
		CSF_IGESDefaults: string;
		TERMINAL_CONTAINER_DISK: string;
		TRIBUNAL_BOT_NAME: string;
		CSF_MDTVTexturesDirectory: string;
		_: string;
		TERMINAL_LIFETIME_SECONDS: string;
		CSF_StandardLiteDefaults: string;
		TERMINAL_DOCKER_IMAGE: string;
		XDG_DATA_DIRS: string;
		CASROOT: string;
		TERMINAL_MODAL_MODE: string;
		npm_lifecycle_event: string;
		HERMES_WEBUI_PASSWORD: string;
		LC_NAME: string;
		HERMES_QUIET: string;
		HERMES_WEBUI_AGENT_DIR: string;
		VSSCRIPT_PATH: string;
		npm_lifecycle_script: string;
		HERMES_SESSION_CHAT_ID: string;
		TERMINAL_CONTAINER_PERSISTENT: string;
		CSF_TObjMessage: string;
		AUXILIARY_VISION_MODEL: string;
		MOA_TOOLS_DEBUG: string;
		LC_IDENTIFICATION: string;
		HERMES_CRON_TIMEOUT: string;
		TRIBUNAL_ROLE: string;
		npm_config_global_prefix: string;
		BROWSER_SESSION_TIMEOUT: string;
		npm_package_version: string;
		npm_config_cache: string;
		npm_config_node_gyp: string;
		SHELL: string;
		CSF_STEPDefaults: string;
		TERMINAL_DOCKER_FORWARD_ENV: string;
		DRAWDEFAULT: string;
		HERMES_EXEC_ASK: string;
		NODE_ENV: string;
		MATRIX_ENCRYPTION: string;
		CSF_DrawPluginDefaults: string;
		CSF_LANGUAGE: string;
		LANG: string;
		TERMINAL_DOCKER_EXTRA_ARGS: string;
		TERMINAL_CONTAINER_CPU: string;
		NODE: string;
		TERMINAL_SINGULARITY_IMAGE: string;
		HERMES_WEBUI_STATE_DIR: string;
		INVOCATION_ID: string;
		CSF_SHMessage: string;
		EDITOR: string;
		TERMINAL_TIMEOUT: string;
		HERMES_SESSION_ID: string;
		COLOR: string;
		TRIBUNAL_AGENT_ID: string;
		npm_command: string;
		LOGNAME: string;
		HERMES_REAL_HOME: string;
		MEMORY_PRESSURE_WATCH: string;
		npm_config_userconfig: string;
		IMAGE_TOOLS_DEBUG: string;
		npm_config_init_module: string;
		npm_execpath: string;
		TERMINAL_MODAL_IMAGE: string;
		WEB_TOOLS_DEBUG: string;
		SYSTEMD_EXEC_PID: string;
		AUXILIARY_VISION_PROVIDER: string;
		CSF_StandardDefaults: string;
		HOME: string;
		LC_TELEPHONE: string;
		LC_PAPER: string;
		PWD: string;
		HERMES_SESSION_PLATFORM: string;
		LC_ADDRESS: string;
		TERMINAL_DOCKER_RUN_AS_HOST_USER: string;
		[key: `PUBLIC_${string}`]: undefined;
		[key: `${string}`]: string | undefined;
	}
}

/**
 * This module provides access to environment variables set _dynamically_ at runtime and that are _publicly_ accessible.
 * 
 * |         | Runtime                                                                    | Build time                                                               |
 * | ------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
 * | Private | [`$env/dynamic/private`](https://svelte.dev/docs/kit/$env-dynamic-private) | [`$env/static/private`](https://svelte.dev/docs/kit/$env-static-private) |
 * | Public  | [`$env/dynamic/public`](https://svelte.dev/docs/kit/$env-dynamic-public)   | [`$env/static/public`](https://svelte.dev/docs/kit/$env-static-public)   |
 * 
 * Dynamic environment variables are defined by the platform you're running on. For example if you're using [`adapter-node`](https://github.com/sveltejs/kit/tree/main/packages/adapter-node) (or running [`vite preview`](https://svelte.dev/docs/kit/cli)), this is equivalent to `process.env`.
 * 
 * **_Public_ access:**
 * 
 * - This module _can_ be imported into client-side code
 * - **Only** variables that begin with [`config.kit.env.publicPrefix`](https://svelte.dev/docs/kit/configuration#env) (which defaults to `PUBLIC_`) are included
 * 
 * > [!NOTE] In `dev`, `$env/dynamic` includes environment variables from `.env`. In `prod`, this behavior will depend on your adapter.
 * 
 * > [!NOTE] To get correct types, environment variables referenced in your code should be declared (for example in an `.env` file), even if they don't have a value until the app is deployed:
 * >
 * > ```env
 * > MY_FEATURE_FLAG=
 * > ```
 * >
 * > You can override `.env` values from the command line like so:
 * >
 * > ```sh
 * > MY_FEATURE_FLAG="enabled" npm run dev
 * > ```
 * 
 * For example, given the following runtime environment:
 * 
 * ```env
 * ENVIRONMENT=production
 * PUBLIC_BASE_URL=http://example.com
 * ```
 * 
 * With the default `publicPrefix` and `privatePrefix`:
 * 
 * ```ts
 * import { env } from '$env/dynamic/public';
 * console.log(env.ENVIRONMENT); // => undefined, not public
 * console.log(env.PUBLIC_BASE_URL); // => "http://example.com"
 * ```
 * 
 * ```
 * 
 * ```
 */
declare module '$env/dynamic/public' {
	export const env: {
		[key: `PUBLIC_${string}`]: string | undefined;
	}
}
