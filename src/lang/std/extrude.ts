import { InternalFn } from './stdTypes'
import {
  ExtrudeGroup,
  ExtrudeSurface,
  SketchGroup,
  Position,
  Rotation,
} from '../executor'
import { clockwiseSign } from './std'
import { generateUuidFromHashSeed } from '../../lib/uuid'

export const extrude: InternalFn = (
  { sourceRange, engineCommandManager, code },
  length: number,
  sketchVal: SketchGroup
): ExtrudeGroup => {
  const sketch = sketchVal
  const { position, rotation } = sketchVal

  const id = generateUuidFromHashSeed(
    JSON.stringify({
      code,
      sourceRange,
      data: {
        length,
        sketchVal,
      },
    })
  )

  const extrudeSurfaces: ExtrudeSurface[] = []
  const extrusionDirection = clockwiseSign(sketch.value.map((line) => line.to))
  engineCommandManager.sendModelingCommand({
    id,
    range: sourceRange,
    command: {
      type: 'modeling_cmd_req',
      cmd: {
        type: 'extrude',
        target: sketch.id,
        distance: length,
        cap: true,
      },
      cmd_id: id,
    },
  })

  return {
    type: 'extrudeGroup',
    id,
    value: extrudeSurfaces, // TODO, this is just an empty array now, should be deleted.
    height: length,
    position,
    rotation,
    __meta: [
      {
        sourceRange,
        pathToNode: [], // TODO
      },
      {
        sourceRange: sketchVal.__meta[0].sourceRange,
        pathToNode: sketchVal.__meta[0].pathToNode,
      },
    ],
  }
}

export const getExtrudeWallTransform: InternalFn = (
  _,
  pathName: string,
  extrudeGroup: ExtrudeGroup
): {
  position: Position
  quaternion: Rotation
} => {
  const path = extrudeGroup?.value.find((path) => path.name === pathName)
  if (!path) throw new Error(`Could not find path with name ${pathName}`)
  return {
    position: path.position,
    quaternion: path.rotation,
  }
}
