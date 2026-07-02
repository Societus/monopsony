
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
	export const HERMES_HOME: string;
	export const OLDPWD: string;
	export const LC_NUMERIC: string;
	export const npm_node_execpath: string;
	export const DRAWDEFAULT: string;
	export const TERMINAL_DOCKER_IMAGE: string;
	export const HERMES_KANBAN_BOARD: string;
	export const LC_PAPER: string;
	export const HERMES_SESSION_PLATFORM: string;
	export const PWD: string;
	export const LC_ADDRESS: string;
	export const SSH_TTY: string;
	export const HOME: string;
	export const CSF_StandardDefaults: string;
	export const MOTD_SHOWN: string;
	export const AUXILIARY_VISION_PROVIDER: string;
	export const WEB_TOOLS_DEBUG: string;
	export const TERMINAL_MODAL_IMAGE: string;
	export const TERMINAL_DOCKER_RUN_AS_HOST_USER: string;
	export const npm_config_init_module: string;
	export const IMAGE_TOOLS_DEBUG: string;
	export const XDG_SESSION_TYPE: string;
	export const npm_config_userconfig: string;
	export const HERMES_SESSION_ID: string;
	export const npm_command: string;
	export const TRIBUNAL_AGENT_ID: string;
	export const COLOR: string;
	export const LS_COLORS: string;
	export const TERMINAL_CONTAINER_CPU: string;
	export const TERMINAL_TIMEOUT: string;
	export const LANG: string;
	export const CSF_LANGUAGE: string;
	export const CSF_DrawPluginDefaults: string;
	export const MATRIX_ENCRYPTION: string;
	export const HERMES_EXEC_ASK: string;
	export const CSF_STEPDefaults: string;
	export const NODE_ENV: string;
	export const HERMES_WEBUI_PRESERVE_ENV: string;
	export const npm_package_version: string;
	export const BROWSER_SESSION_TIMEOUT: string;
	export const npm_config_global_prefix: string;
	export const TRIBUNAL_ROLE: string;
	export const HERMES_WEBUI_STATE_DIR: string;
	export const TERMINAL_LIFETIME_SECONDS: string;
	export const HERMES_SESSION_CHAT_ID: string;
	export const npm_lifecycle_script: string;
	export const AUXILIARY_VISION_API_KEY: string;
	export const HERMES_WEBUI_AGENT_DIR: string;
	export const HERMES_INTERACTIVE: string;
	export const HERMES_QUIET: string;
	export const LC_TELEPHONE: string;
	export const EDITOR: string;
	export const CSF_SHMessage: string;
	export const LC_NAME: string;
	export const MOA_TOOLS_DEBUG: string;
	export const HERMES_WEBUI_PASSWORD: string;
	export const LOGNAME: string;
	export const SHELL: string;
	export const npm_config_node_gyp: string;
	export const SSH_CONNECTION: string;
	export const TERMINAL_MODAL_MODE: string;
	export const CASROOT: string;
	export const XDG_DATA_DIRS: string;
	export const _: string;
	export const TRIBUNAL_BOT_NAME: string;
	export const TERMINAL_CONTAINER_DISK: string;
	export const CSF_IGESDefaults: string;
	export const HERMES_WEBUI_HOST: string;
	export const CSF_OCCTResourcePath: string;
	export const LC_TIME: string;
	export const HERMES_CRON_TIMEOUT: string;
	export const LC_IDENTIFICATION: string;
	export const AUXILIARY_VISION_MODEL: string;
	export const CSF_TObjMessage: string;
	export const TERMINAL_CONTAINER_PERSISTENT: string;
	export const DRAWHOME: string;
	export const TERMINAL_DOCKER_FORWARD_ENV: string;
	export const TERMINAL_SINGULARITY_IMAGE: string;
	export const CSF_MIGRATION_TYPES: string;
	export const npm_config_globalconfig: string;
	export const NODE: string;
	export const npm_config_cache: string;
	export const TERM: string;
	export const MAIL: string;
	export const INIT_CWD: string;
	export const DEBUGINFOD_URLS: string;
	export const CSF_ShadersDirectory: string;
	export const BROWSERBASE_PROXIES: string;
	export const CSF_PluginDefaults: string;
	export const CSF_EXCEPTION_PROMPT: string;
	export const npm_config_user_agent: string;
	export const CSF_XmlOcafResource: string;
	export const VISION_TOOLS_DEBUG: string;
	export const TERMINAL_DOCKER_ENV: string;
	export const npm_config_npm_version: string;
	export const XDG_SESSION_CLASS: string;
	export const CSF_MDTVTexturesDirectory: string;
	export const BROWSER_INACTIVITY_TIMEOUT: string;
	export const npm_config_local_prefix: string;
	export const npm_package_name: string;
	export const LC_MEASUREMENT: string;
	export const LC_MONETARY: string;
	export const npm_config_prefix: string;
	export const USER: string;
	export const CSF_XCAFDefaults: string;
	export const SHLVL: string;
	export const TERMINAL_DAYTONA_IMAGE: string;
	export const XDG_SESSION_ID: string;
	export const AUXILIARY_VISION_BASE_URL: string;
	export const TERMINAL_PERSISTENT_SHELL: string;
	export const TERMINAL_HOME_MODE: string;
	export const CSF_StandardLiteDefaults: string;
	export const TERMINAL_CWD: string;
	export const XDG_RUNTIME_DIR: string;
	export const SSH_CLIENT: string;
	export const npm_package_json: string;
	export const DBUS_SESSION_BUS_ADDRESS: string;
	export const DOCKER_HOST: string;
	export const BUN_INSTALL: string;
	export const TERMINAL_DOCKER_VOLUMES: string;
	export const PATH: string;
	export const HERMES_REDACT_SECRETS: string;
	export const SVELTEKIT_FORK: string;
	export const TERMINAL_DOCKER_MOUNT_CWD_TO_WORKSPACE: string;
	export const npm_lifecycle_event: string;
	export const CSF_XSMessage: string;
	export const MMGT_CLEAR: string;
	export const npm_execpath: string;
	export const TERMINAL_ENV: string;
	export const HERMES_MAX_ITERATIONS: string;
	export const HERMES_WEBUI_PORT: string;
	export const npm_config_noproxy: string;
	export const CSF_TObjDefaults: string;
	export const HERMES_REAL_HOME: string;
	export const TERMINAL_CONTAINER_MEMORY: string;
	export const BROWSERBASE_ADVANCED_STEALTH: string;
	export const HERMES_SESSION_KEY: string;
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
		HERMES_HOME: string;
		OLDPWD: string;
		LC_NUMERIC: string;
		npm_node_execpath: string;
		DRAWDEFAULT: string;
		TERMINAL_DOCKER_IMAGE: string;
		HERMES_KANBAN_BOARD: string;
		LC_PAPER: string;
		HERMES_SESSION_PLATFORM: string;
		PWD: string;
		LC_ADDRESS: string;
		SSH_TTY: string;
		HOME: string;
		CSF_StandardDefaults: string;
		MOTD_SHOWN: string;
		AUXILIARY_VISION_PROVIDER: string;
		WEB_TOOLS_DEBUG: string;
		TERMINAL_MODAL_IMAGE: string;
		TERMINAL_DOCKER_RUN_AS_HOST_USER: string;
		npm_config_init_module: string;
		IMAGE_TOOLS_DEBUG: string;
		XDG_SESSION_TYPE: string;
		npm_config_userconfig: string;
		HERMES_SESSION_ID: string;
		npm_command: string;
		TRIBUNAL_AGENT_ID: string;
		COLOR: string;
		LS_COLORS: string;
		TERMINAL_CONTAINER_CPU: string;
		TERMINAL_TIMEOUT: string;
		LANG: string;
		CSF_LANGUAGE: string;
		CSF_DrawPluginDefaults: string;
		MATRIX_ENCRYPTION: string;
		HERMES_EXEC_ASK: string;
		CSF_STEPDefaults: string;
		NODE_ENV: string;
		HERMES_WEBUI_PRESERVE_ENV: string;
		npm_package_version: string;
		BROWSER_SESSION_TIMEOUT: string;
		npm_config_global_prefix: string;
		TRIBUNAL_ROLE: string;
		HERMES_WEBUI_STATE_DIR: string;
		TERMINAL_LIFETIME_SECONDS: string;
		HERMES_SESSION_CHAT_ID: string;
		npm_lifecycle_script: string;
		AUXILIARY_VISION_API_KEY: string;
		HERMES_WEBUI_AGENT_DIR: string;
		HERMES_INTERACTIVE: string;
		HERMES_QUIET: string;
		LC_TELEPHONE: string;
		EDITOR: string;
		CSF_SHMessage: string;
		LC_NAME: string;
		MOA_TOOLS_DEBUG: string;
		HERMES_WEBUI_PASSWORD: string;
		LOGNAME: string;
		SHELL: string;
		npm_config_node_gyp: string;
		SSH_CONNECTION: string;
		TERMINAL_MODAL_MODE: string;
		CASROOT: string;
		XDG_DATA_DIRS: string;
		_: string;
		TRIBUNAL_BOT_NAME: string;
		TERMINAL_CONTAINER_DISK: string;
		CSF_IGESDefaults: string;
		HERMES_WEBUI_HOST: string;
		CSF_OCCTResourcePath: string;
		LC_TIME: string;
		HERMES_CRON_TIMEOUT: string;
		LC_IDENTIFICATION: string;
		AUXILIARY_VISION_MODEL: string;
		CSF_TObjMessage: string;
		TERMINAL_CONTAINER_PERSISTENT: string;
		DRAWHOME: string;
		TERMINAL_DOCKER_FORWARD_ENV: string;
		TERMINAL_SINGULARITY_IMAGE: string;
		CSF_MIGRATION_TYPES: string;
		npm_config_globalconfig: string;
		NODE: string;
		npm_config_cache: string;
		TERM: string;
		MAIL: string;
		INIT_CWD: string;
		DEBUGINFOD_URLS: string;
		CSF_ShadersDirectory: string;
		BROWSERBASE_PROXIES: string;
		CSF_PluginDefaults: string;
		CSF_EXCEPTION_PROMPT: string;
		npm_config_user_agent: string;
		CSF_XmlOcafResource: string;
		VISION_TOOLS_DEBUG: string;
		TERMINAL_DOCKER_ENV: string;
		npm_config_npm_version: string;
		XDG_SESSION_CLASS: string;
		CSF_MDTVTexturesDirectory: string;
		BROWSER_INACTIVITY_TIMEOUT: string;
		npm_config_local_prefix: string;
		npm_package_name: string;
		LC_MEASUREMENT: string;
		LC_MONETARY: string;
		npm_config_prefix: string;
		USER: string;
		CSF_XCAFDefaults: string;
		SHLVL: string;
		TERMINAL_DAYTONA_IMAGE: string;
		XDG_SESSION_ID: string;
		AUXILIARY_VISION_BASE_URL: string;
		TERMINAL_PERSISTENT_SHELL: string;
		TERMINAL_HOME_MODE: string;
		CSF_StandardLiteDefaults: string;
		TERMINAL_CWD: string;
		XDG_RUNTIME_DIR: string;
		SSH_CLIENT: string;
		npm_package_json: string;
		DBUS_SESSION_BUS_ADDRESS: string;
		DOCKER_HOST: string;
		BUN_INSTALL: string;
		TERMINAL_DOCKER_VOLUMES: string;
		PATH: string;
		HERMES_REDACT_SECRETS: string;
		SVELTEKIT_FORK: string;
		TERMINAL_DOCKER_MOUNT_CWD_TO_WORKSPACE: string;
		npm_lifecycle_event: string;
		CSF_XSMessage: string;
		MMGT_CLEAR: string;
		npm_execpath: string;
		TERMINAL_ENV: string;
		HERMES_MAX_ITERATIONS: string;
		HERMES_WEBUI_PORT: string;
		npm_config_noproxy: string;
		CSF_TObjDefaults: string;
		HERMES_REAL_HOME: string;
		TERMINAL_CONTAINER_MEMORY: string;
		BROWSERBASE_ADVANCED_STEALTH: string;
		HERMES_SESSION_KEY: string;
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
