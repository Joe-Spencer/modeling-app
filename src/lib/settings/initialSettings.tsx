import { DEFAULT_PROJECT_NAME } from 'lib/constants'
import {
  BaseUnit,
  SettingProps,
  SettingsLevel,
  baseUnitsUnion,
} from 'lib/settings/settingsTypes'
import { Themes } from 'lib/theme'
import { isEnumMember } from 'lib/types'
import {
  CameraSystem,
  cameraMouseDragGuards,
  cameraSystems,
} from 'lib/cameraControls'
import { isTauri } from 'lib/isTauri'
import { useRef } from 'react'
import { open } from '@tauri-apps/api/dialog'
import { CustomIcon } from 'components/CustomIcon'
import Tooltip from 'components/Tooltip'

/**
 * A setting that can be set at the user or project level
 * @constructor
 */
export class Setting<T = unknown> {
  /**
   * The current value of the setting, prioritizing project, then user, then default
   */
  public current: T
  public hideOnLevel: SettingProps<T>['hideOnLevel']
  public hideOnPlatform: SettingProps<T>['hideOnPlatform']
  public commandConfig: SettingProps<T>['commandConfig']
  public Component: SettingProps<T>['Component']
  public description?: string
  private validate: (v: T) => boolean
  private _default: T
  private _user?: T
  private _project?: T

  constructor(props: SettingProps<T>) {
    this._default = props.defaultValue
    this.current = props.defaultValue
    this.validate = props.validate
    this.description = props.description
    this.hideOnLevel = props.hideOnLevel
    this.hideOnPlatform = props.hideOnPlatform
    this.commandConfig = props.commandConfig
    this.Component = props.Component
  }
  /**
   * The default setting. Overridden by the user and project if set
   */
  get default(): T {
    return this._default
  }
  set default(v: T) {
    this._default = this.validate(v) ? v : this._default
    this.current = this.resolve()
  }
  /**
   * The user-level setting. Overrides the default, overridden by the project
   */
  get user(): T | undefined {
    return this._user
  }
  set user(v: T) {
    this._user = this.validate(v) ? v : this._user
    this.current = this.resolve()
  }
  /**
   * The project-level setting. Overrides the user and default
   */
  get project(): T | undefined {
    return this._project
  }
  set project(v: T) {
    this._project = this.validate(v) ? v : this._project
    this.current = this.resolve()
  }
  /**
   * @returns {T} - The value of the setting, prioritizing project, then user, then default
   * @todo - This may have issues if future settings can have a value that is valid but falsy
   */
  private resolve() {
    return this._project !== undefined
      ? this._project
      : this._user !== undefined
      ? this._user
      : this._default
  }
  /**
   * @param {SettingsLevel} level - The level to get the fallback for
   * @returns {T} - The value of the setting above the given level, falling back as needed
   */
  public getFallback(level: SettingsLevel | 'default'): T {
    return level === 'project'
      ? this._user !== undefined
        ? this._user
        : this._default
      : this._default
  }
  public getParentLevel(level: SettingsLevel): SettingsLevel | 'default' {
    return level === 'project' ? 'user' : 'default'
  }
}

export function createSettings() {
  return {
    /** Settings that affect the behavior of the entire app,
     *  beyond just modeling or navigating, for example
     */
    app: {
      /**
       * The overall appearance of the app: light, dark, or system
       */
      theme: new Setting<Themes>({
        defaultValue: Themes.System,
        description: 'The overall appearance of the app',
        validate: (v) => isEnumMember(v, Themes),
        commandConfig: {
          inputType: 'options',
          defaultValueFromContext: (context) => context.app.theme.current,
          options: (cmdContext, settingsContext) =>
            Object.values(Themes).map((v) => ({
              name: v,
              value: v,
              isCurrent:
                v ===
                settingsContext.app.theme[
                  cmdContext.argumentsToSubmit.level as SettingsLevel
                ],
            })),
        },
      }),
      onboardingStatus: new Setting<string>({
        defaultValue: '',
        validate: (v) => typeof v === 'string',
      }),
      projectDirectory: new Setting<string>({
        defaultValue: '',
        description: 'The directory to save and load projects from',
        hideOnLevel: 'project',
        hideOnPlatform: 'web',
        validate: (v) => typeof v === 'string' && (v.length > 0 || !isTauri()),
        Component: ({ value, onChange }) => {
          const inputRef = useRef<HTMLInputElement>(null)
          return (
            <div className="flex gap-4 p-1 border rounded-sm border-chalkboard-30">
              <input
                className="flex-grow text-xs px-2 bg-transparent"
                value={value}
                onBlur={onChange}
                disabled
                data-testid="default-directory-input"
              />
              <button
                onClick={async () => {
                  const newValue = await open({
                    directory: true,
                    recursive: true,
                    defaultPath: value,
                    title: 'Choose a new default directory',
                  })
                  if (
                    inputRef.current &&
                    newValue &&
                    newValue !== null &&
                    !Array.isArray(newValue)
                  ) {
                    inputRef.current.value = newValue
                  }
                }}
                className="p-0 m-0 border-none hover:bg-energy-10 focus:bg-energy-10 dark:hover:bg-energy-80/50 dark:focus::bg-energy-80/50"
              >
                <CustomIcon name="folder" className="w-5 h-5" />
                <Tooltip position="inlineStart">Choose a folder</Tooltip>
              </button>
            </div>
          )
        },
      }),
    },
    /**
     * Settings that affect the behavior while modeling.
     */
    modeling: {
      /**
       * The default unit to use in modeling dimensions
       */
      defaultUnit: new Setting<BaseUnit>({
        defaultValue: 'mm',
        description: 'The default unit to use in modeling dimensions',
        validate: (v) => baseUnitsUnion.includes(v as BaseUnit),
        commandConfig: {
          inputType: 'options',
          defaultValueFromContext: (context) =>
            context.modeling.defaultUnit.current,
          options: (cmdContext, settingsContext) =>
            Object.values(baseUnitsUnion).map((v) => ({
              name: v,
              value: v,
              isCurrent:
                v ===
                settingsContext.modeling.defaultUnit[
                  cmdContext.argumentsToSubmit.level as SettingsLevel
                ],
            })),
        },
      }),
      /**
       * The controls for how to navigate the 3D view
       */
      mouseControls: new Setting<CameraSystem>({
        defaultValue: 'KittyCAD',
        description: 'The controls for how to navigate the 3D view',
        validate: (v) => cameraSystems.includes(v as CameraSystem),
        hideOnLevel: 'project',
        commandConfig: {
          inputType: 'options',
          defaultValueFromContext: (context) =>
            context.modeling.mouseControls.current,
          options: (cmdContext, settingsContext) =>
            Object.values(cameraSystems).map((v) => ({
              name: v,
              value: v,
              isCurrent:
                v ===
                settingsContext.modeling.mouseControls[
                  cmdContext.argumentsToSubmit.level as SettingsLevel
                ],
            })),
        },
        Component: ({ value, onChange }) => (
          <>
            <select
              id="camera-controls"
              className="block w-full px-3 py-1 bg-transparent border border-chalkboard-30"
              value={value}
              onChange={onChange}
            >
              {cameraSystems.map((program) => (
                <option key={program} value={program}>
                  {program}
                </option>
              ))}
            </select>
            <ul className="mx-0 my-2 flex flex-col gap-2 text-sm">
              <li className="grid grid-cols-4 gap-1">
                <strong>Pan</strong>
                <p className="col-span-3 leading-tight">
                  {cameraMouseDragGuards[value].pan.description}
                </p>
              </li>
              <li className="grid grid-cols-4 gap-1">
                <strong>Zoom</strong>
                <p className="col-span-3 leading-tight">
                  {cameraMouseDragGuards[value].zoom.description}
                </p>
              </li>
              <li className="grid grid-cols-4 gap-1">
                <strong>Rotate</strong>
                <p className="col-span-3 leading-tight">
                  {cameraMouseDragGuards[value].rotate.description}
                </p>
              </li>
            </ul>
          </>
        ),
      }),
      /**
       * Whether to show the debug panel, which lets you see
       * various states of the app to aid in development
       */
      showDebugPanel: new Setting<boolean>({
        defaultValue: false,
        description: 'Whether to show the debug panel, a development tool',
        validate: (v) => typeof v === 'boolean',
        commandConfig: {
          inputType: 'boolean',
        },
      }),
      /**
       * TODO: This setting is not yet implemented.
       * Whether to turn off animations and other motion effects
       */
      // reduceMotion: new Setting<boolean>({
      //   defaultValue: false,
      //   description: 'Whether to turn off animations and other motion effects',
      //   validate: (v) => typeof v === 'boolean',
      //   commandConfig: {
      //     inputType: 'boolean',
      //   },
      //   hideOnLevel: 'project',
      // }),
      /**
       * TODO: This setting is not yet implemented.
       * Whether to move to view the sketch plane orthogonally
       * when creating entering or creating a sketch.
       */
      // moveOrthoginalToSketch: new Setting<boolean>({
      //   defaultValue: false,
      //   description: 'Whether to move to view sketch planes orthogonally',
      //   validate: (v) => typeof v === 'boolean',
      //   commandConfig: {
      //     inputType: 'boolean',
      //   },
      // }),
    },
    /**
     * Settings that affect the behavior of the KCL text editor.
     */
    textEditor: {
      /**
       * Whether to wrap text in the editor or overflow with scroll
       */
      textWrapping: new Setting<boolean>({
        defaultValue: true,
        description:
          'Whether to wrap text in the editor or overflow with scroll',
        validate: (v) => typeof v === 'boolean',
        commandConfig: {
          inputType: 'boolean',
        },
      }),
    },
    /**
     * Settings that affect the behavior of project management.
     */
    projects: {
      /**
       * The default project name to use when creating a new project
       */
      defaultProjectName: new Setting<string>({
        defaultValue: DEFAULT_PROJECT_NAME,
        description:
          'The default project name to use when creating a new project',
        validate: (v) => typeof v === 'string' && v.length > 0,
        commandConfig: {
          inputType: 'string',
          defaultValueFromContext: (context) =>
            context.projects.defaultProjectName.current,
        },
        hideOnLevel: 'project',
        hideOnPlatform: 'web',
      }),
      /**
       * TODO: This setting is not yet implemented.
       * It requires more sophisticated fallback logic if the user sets this setting to a
       * non-existent file. This setting is currently hardcoded to PROJECT_ENTRYPOINT.
       * The default file to open when a project is loaded
       */
      // entryPointFileName: new Setting<string>({
      //   defaultValue: PROJECT_ENTRYPOINT,
      //   description: 'The default file to open when a project is loaded',
      //   validate: (v) => typeof v === 'string' && v.length > 0,
      //   commandConfig: {
      //     inputType: 'string',
      //     defaultValueFromContext: (context) =>
      //       context.projects.entryPointFileName.current,
      //   },
      //   hideOnLevel: 'project',
      // }),
    },
    /**
     * Settings that affect the behavior of the command bar.
     */
    commandBar: {
      /**
       * Whether to include settings in the command bar
       */
      includeSettings: new Setting<boolean>({
        defaultValue: true,
        description: 'Whether to include settings in the command bar',
        validate: (v) => typeof v === 'boolean',
        commandConfig: {
          inputType: 'boolean',
        },
      }),
    },
  }
}

export const settings = createSettings()