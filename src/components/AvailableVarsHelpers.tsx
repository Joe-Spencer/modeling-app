import { useEffect, useState, useRef } from 'react'
import {
  abstractSyntaxTree,
  BinaryPart,
  Value,
} from '../lang/abstractSyntaxTree'
import { executor } from '../lang/executor'
import {
  createIdentifier,
  createLiteral,
  createUnaryExpression,
  findUniqueName,
} from '../lang/modifyAst'
import { findAllPreviousVariables, PrevVariable } from '../lang/queryAst'
import { lexer } from '../lang/tokeniser'
import { useStore } from '../useStore'

export const AvailableVars = ({
  onVarClick,
  prevVariables,
}: {
  onVarClick: (a: string) => void
  prevVariables: PrevVariable<any>[]
}) => {
  return (
    <ul className="flex flex-col">
      {prevVariables.length &&
        prevVariables.map(({ key, value }) => (
          <li key={key}>
            <button
              className="flex w-full justify-between items-center rounded-md hover:bg-gray-100 max-w-xs"
              onClick={() => onVarClick(key)}
            >
              <span className="font-[monospace] text-gray-800">{key}</span>{' '}
              <span className="font-[monospace] text-gray-600 w-24 text-start font-bold">
                {value}
              </span>
            </button>
          </li>
        ))}
    </ul>
  )
}

export const addToInputHelper =
  (
    inputRef: React.RefObject<HTMLInputElement>,
    setValue: (a: string) => void
  ) =>
  (varName: string) => {
    const selectionStart = inputRef.current?.selectionStart
    let selectionEnd = inputRef.current?.selectionEnd
    let newValue = ''
    if (
      typeof selectionStart === 'number' &&
      typeof selectionEnd === 'number'
    ) {
      newValue = stringSplice(
        inputRef.current?.value || '',
        selectionStart,
        selectionEnd,
        varName
      )
      selectionEnd = selectionStart + varName.length
    } else {
      newValue = inputRef.current?.value + varName
    }
    setValue(newValue)
    inputRef.current?.focus()
    setTimeout(() => {
      // run in the next render cycle
      const _selectionEnd =
        typeof selectionEnd === 'number' ? selectionEnd : newValue.length
      inputRef.current?.setSelectionRange(_selectionEnd, _selectionEnd)
    })
  }

function stringSplice(str: string, index: number, count: number, add: string) {
  return str.slice(0, index) + (add || '') + str.slice(index + count)
}

export function useCalc({
  value,
  initialVariableName: valueName = '',
}: {
  value: string
  initialVariableName?: string
}): {
  inputRef: React.RefObject<HTMLInputElement>
  valueNode: Value | null
  calcResult: string
  prevVariables: PrevVariable<any>[]
  newVariableName: string
  isNewVariableNameUnique: boolean
  newVariableInsertIndex: number
  setNewVariableName: (a: string) => void
} {
  const { ast, programMemory, selectionRange } = useStore((s) => ({
    ast: s.ast,
    programMemory: s.programMemory,
    selectionRange: s.selectionRanges[0],
  }))
  const inputRef = useRef<HTMLInputElement>(null)
  const [availableVarInfo, setAvailableVarInfo] = useState<
    ReturnType<typeof findAllPreviousVariables>
  >({
    variables: [],
    insertIndex: 0,
    bodyPath: [],
  })
  const [valueNode, setValueNode] = useState<Value | null>(null)
  const [calcResult, setCalcResult] = useState('NAN')
  const [newVariableName, setNewVariableName] = useState('')
  const [isNewVariableNameUnique, setIsNewVariableNameUnique] = useState(true)

  useEffect(() => {
    setTimeout(() => {
      inputRef.current && inputRef.current.focus()
      inputRef.current &&
        inputRef.current.setSelectionRange(0, String(value).length)
    }, 100)
    if (ast) {
      setNewVariableName(findUniqueName(ast, valueName))
    }
  }, [])

  useEffect(() => {
    const allVarNames = Object.keys(programMemory.root)
    if (allVarNames.includes(newVariableName)) {
      setIsNewVariableNameUnique(false)
    } else {
      setIsNewVariableNameUnique(true)
    }
  }, [newVariableName])

  useEffect(() => {
    if (!ast || !programMemory || !selectionRange) return
    const varInfo = findAllPreviousVariables(ast, programMemory, selectionRange)
    setAvailableVarInfo(varInfo)
  }, [ast, programMemory, selectionRange])

  useEffect(() => {
    try {
      const code = `const __result__ = ${value}\nshow(__result__)`
      const ast = abstractSyntaxTree(lexer(code))
      const _programMem: any = { root: {} }
      availableVarInfo.variables.forEach(({ key, value }) => {
        _programMem.root[key] = { type: 'userVal', value, __meta: [] }
      })
      const programMemory = executor(ast, _programMem)
      const resultDeclaration = ast.body.find(
        (a) =>
          a.type === 'VariableDeclaration' &&
          a.declarations?.[0]?.id?.name === '__result__'
      )
      const init =
        resultDeclaration?.type === 'VariableDeclaration' &&
        resultDeclaration?.declarations?.[0]?.init
      const result = programMemory?.root?.__result__?.value
      setCalcResult(typeof result === 'number' ? String(result) : 'NAN')
      init && setValueNode(init)
    } catch (e) {
      setCalcResult('NAN')
      setValueNode(null)
    }
  }, [value])

  return {
    valueNode,
    calcResult,
    prevVariables: availableVarInfo.variables,
    newVariableInsertIndex: availableVarInfo.insertIndex,
    newVariableName,
    isNewVariableNameUnique,
    setNewVariableName,
    inputRef,
  }
}

export const CalcResult = ({ calcResult }: { calcResult: string }) => {
  return (
    <div className="font-[monospace] pl-4 text-gray-600">
      <span
        className={`${
          calcResult === 'NAN' ? 'bg-pink-200' : ''
        } px-2 py-0.5 rounded`}
      >
        = {calcResult}
      </span>
    </div>
  )
}

export const CreateNewVariable = ({
  newVariableName,
  isNewVariableNameUnique,
  setNewVariableName,
  shouldCreateVariable,
  setShouldCreateVariable,
  showCheckbox = true,
}: {
  isNewVariableNameUnique: boolean
  newVariableName: string
  setNewVariableName: (a: string) => void
  shouldCreateVariable: boolean
  setShouldCreateVariable: (a: boolean) => void
  showCheckbox?: boolean
}) => {
  return (
    <>
      <label
        htmlFor="create-new-variable"
        className="block text-sm font-medium text-gray-700 mt-3 font-mono"
      >
        Create new variable
      </label>
      <div className="mt-1 flex flex-1">
        {showCheckbox && (
          <input
            type="checkbox"
            className="shadow-sm focus:ring-blue-500 focus:border-blue-500 block w-full sm:text-sm border-gray-300 rounded-md font-mono pl-1 flex-shrink"
            checked={shouldCreateVariable}
            onChange={(e) => {
              setShouldCreateVariable(e.target.checked)
            }}
          />
        )}
        <input
          type="text"
          disabled={!shouldCreateVariable}
          name="create-new-variable"
          id="create-new-variable"
          className={`shadow-sm font-[monospace] focus:ring-blue-500 focus:border-blue-500 block w-full sm:text-sm border-gray-300 rounded-md font-mono pl-1 flex-shrink-0 ${
            !shouldCreateVariable ? 'opacity-50' : ''
          }`}
          value={newVariableName}
          onChange={(e) => {
            setNewVariableName(e.target.value)
          }}
        />
      </div>
      {!isNewVariableNameUnique && (
        <div className="bg-pink-200 rounded px-2 py-0.5 text-xs">
          Sorry, that's not a unique variable name. Please try something else
        </div>
      )}
    </>
  )
}

export function removeDoubleNegatives(
  valueNode: BinaryPart,
  sign: number,
  variableName?: string
): BinaryPart {
  let finValue: BinaryPart = variableName
    ? createIdentifier(variableName)
    : valueNode
  if (sign === -1) finValue = createUnaryExpression(finValue)
  if (
    finValue.type === 'UnaryExpression' &&
    finValue.operator === '-' &&
    finValue.argument.type === 'UnaryExpression' &&
    finValue.argument.operator === '-'
  ) {
    finValue = finValue.argument.argument
  }
  if (
    finValue.type === 'UnaryExpression' &&
    finValue.operator === '-' &&
    finValue.argument.type === 'Literal' &&
    typeof finValue.argument.value === 'number' &&
    finValue.argument.value < 0
  ) {
    finValue = createLiteral(-finValue.argument.value)
  }
  return finValue
}
