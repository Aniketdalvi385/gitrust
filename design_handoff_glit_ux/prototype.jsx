/* global React, ReactDOM */
const { useReducer, useEffect, useRef, useCallback, useState } = React;

// ============================================================================
// constants
// ============================================================================

const DEFAULT_MESSAGE = "fix(README): add conflicting thing to README.md";

const PLAN = {
  command: 'glit "commit my login changes"',
  steps: [
    {
      id: 1,
      verb: "stage",
      pastVerb: "staged",
      target: "README.md",
      stat: "+4 −1",
      marker: null,
      editable: false,
      takesMs: 600,
    },
    {
      id: 2,
      verb: "commit",
      pastVerb: "committed",
      target: null, // message rendered separately
      stat: null,
      marker: "caution",
      editable: true,
      takesMs: 800,
    },
    {
      id: 3,
      verb: "push",
      pastVerb: "pushed",
      target: "→ origin/main",
      stat: null,
      marker: "caution",
      editable: false,
      takesMs: 1400,
    },
  ],
  snapshotSha: "aa44620",
  resultSha: "0de4a0d",
};

const STAGES = [
  { label: "understanding intent", budget: 900 },
  { label: "matching files (12 changed)", budget: 700 },
  { label: "drafting message", budget: 500 },
];

const VARIANTS = {
  A: {
    name: "A · Atomic",
    desc: "One Enter runs the whole plan. Edit message before run. No per-step control.",
    keymap: [
      { k: "Enter", do: "run all" },
      { k: "e", do: "edit commit message" },
      { k: "q", do: "cancel", danger: true },
    ],
  },
  B: {
    name: "B · Per-step",
    desc: "Confirm each step with y/n. Most control; most keystrokes. Best when stakes per-step matter.",
    keymap: [
      { k: "y", do: "do current step" },
      { k: "n", do: "skip current step" },
      { k: "e", do: "edit (on commit step)" },
      { k: "q", do: "abort pipeline", danger: true },
    ],
  },
  C: {
    name: "C · Hybrid",
    desc: "Enter runs all. ↑↓ focus a step; e edits it, s skips it. Bias toward speed with precision when needed.",
    keymap: [
      { k: "Enter", do: "run all" },
      { k: "↑ ↓", do: "focus a step" },
      { k: "e", do: "edit focused step" },
      { k: "s", do: "skip focused step" },
      { k: "q", do: "cancel", danger: true },
    ],
  },
};

// ============================================================================
// reducer
// ============================================================================

const initialState = () => ({
  variant: "C",
  failure: "none", // 'none' | 'hook' | 'push'
  speed: "normal", // 'fast' | 'normal' | 'slow'
  phase: "idle", // 'idle' | 'thinking' | 'plan' | 'running' | 'failed' | 'done' | 'canceled'
  thinkingStage: -1, // -1 = none, 0..n = current
  thinkingMs: [], // actual ms taken per stage
  currentStep: 0,
  stepStatus: ["pending", "pending", "pending"], // 'pending'|'running'|'done'|'skipped'|'failed'
  skipped: [false, false, false],
  focused: 0,
  askingStep: null, // variant B: which step is asking y/n
  message: DEFAULT_MESSAGE,
  editing: false, // boolean: edit modal open
  editValue: "",
  failedStep: null, // index of step that failed
  failedReason: null, // string
});

function reducer(state, action) {
  switch (action.type) {
    case "set-variant":
      return { ...initialState(), variant: action.value, failure: state.failure, speed: state.speed };
    case "set-failure":
      return { ...initialState(), variant: state.variant, failure: action.value, speed: state.speed };
    case "set-speed":
      return { ...state, speed: action.value };
    case "reset":
      return { ...initialState(), variant: state.variant, failure: state.failure, speed: state.speed };

    case "start":
      return {
        ...state,
        phase: "thinking",
        thinkingStage: -1,
        thinkingMs: [],
        currentStep: 0,
        stepStatus: ["pending", "pending", "pending"],
        skipped: [false, false, false],
        focused: 0,
        askingStep: null,
        failedStep: null,
        failedReason: null,
      };
    case "stage-start":
      return { ...state, thinkingStage: action.value };
    case "stage-done": {
      const ms = [...state.thinkingMs];
      ms[action.value] = action.ms;
      return { ...state, thinkingMs: ms };
    }
    case "show-plan":
      return { ...state, phase: "plan", thinkingStage: STAGES.length, focused: 0 };

    case "focus":
      return { ...state, focused: action.value };
    case "toggle-skip": {
      const skipped = [...state.skipped];
      skipped[action.value] = !skipped[action.value];
      return { ...state, skipped };
    }

    case "edit-open":
      return { ...state, editing: true, editValue: state.message };
    case "edit-change":
      return { ...state, editValue: action.value };
    case "edit-save":
      return { ...state, editing: false, message: state.editValue.trim() || state.message };
    case "edit-cancel":
      return { ...state, editing: false, editValue: "" };

    case "exec-start":
      return { ...state, phase: "running", currentStep: 0, askingStep: null };
    case "step-running": {
      const stepStatus = [...state.stepStatus];
      stepStatus[action.value] = "running";
      return { ...state, stepStatus, currentStep: action.value, askingStep: null };
    }
    case "step-done": {
      const stepStatus = [...state.stepStatus];
      stepStatus[action.value] = "done";
      return { ...state, stepStatus };
    }
    case "step-skipped": {
      const stepStatus = [...state.stepStatus];
      stepStatus[action.value] = "skipped";
      return { ...state, stepStatus };
    }
    case "step-failed": {
      const stepStatus = [...state.stepStatus];
      stepStatus[action.value] = "failed";
      return {
        ...state,
        stepStatus,
        phase: "failed",
        failedStep: action.value,
        failedReason: action.reason,
      };
    }
    case "all-done":
      return { ...state, phase: "done", askingStep: null };
    case "ask-step":
      return { ...state, askingStep: action.value, currentStep: action.value };
    case "cancel":
      return { ...state, phase: "canceled", askingStep: null };
    default:
      return state;
  }
}

// ============================================================================
// helpers
// ============================================================================

const speedMult = (s) => (s === "fast" ? 0.3 : s === "slow" ? 2.2 : 1);

const stepLabel = (step, message, status) => {
  if (step.verb === "commit") {
    return { verb: status === "done" ? step.pastVerb : step.verb, scope: `"${message}"` };
  }
  return {
    verb: status === "done" ? step.pastVerb : step.verb,
    target: step.target,
    stat: step.stat,
  };
};

// ============================================================================
// app
// ============================================================================

function App() {
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const variant = VARIANTS[state.variant];

  // ---- thinking stage runner ----
  useEffect(() => {
    if (state.phase !== "thinking") return;
    let cancelled = false;
    let i = 0;

    const advance = () => {
      if (cancelled) return;
      if (i >= STAGES.length) {
        dispatch({ type: "show-plan" });
        return;
      }
      dispatch({ type: "stage-start", value: i });
      const ms = STAGES[i].budget * speedMult(state.speed);
      setTimeout(() => {
        if (cancelled) return;
        dispatch({ type: "stage-done", value: i, ms: Math.round(ms) });
        i++;
        advance();
      }, ms);
    };
    advance();

    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line
  }, [state.phase === "thinking"]);

  // ---- variant A/C run-all execution ----
  useEffect(() => {
    if (state.phase !== "running") return;
    if (state.variant === "B") return;

    let cancelled = false;
    let i = 0;

    const run = () => {
      if (cancelled) return;
      // Skip steps marked skipped
      while (i < PLAN.steps.length && state.skipped[i]) {
        dispatch({ type: "step-skipped", value: i });
        i++;
      }
      if (i >= PLAN.steps.length) {
        dispatch({ type: "all-done" });
        return;
      }

      dispatch({ type: "step-running", value: i });
      const step = PLAN.steps[i];
      const ms = step.takesMs * speedMult(state.speed);

      setTimeout(() => {
        if (cancelled) return;
        if (state.failure === "hook" && step.verb === "commit") {
          dispatch({
            type: "step-failed",
            value: i,
            reason: "pre-commit hook 'cargo fmt --check' failed (exit 1)",
          });
          return;
        }
        if (state.failure === "push" && step.verb === "push") {
          dispatch({
            type: "step-failed",
            value: i,
            reason:
              "push rejected: non-fast-forward — origin/main has 1 new commit since your last fetch",
          });
          return;
        }
        dispatch({ type: "step-done", value: i });
        i++;
        run();
      }, ms);
    };

    run();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line
  }, [state.phase === "running" && state.variant !== "B"]);

  // ---- variant B: kick off first ask ----
  useEffect(() => {
    if (state.phase !== "running") return;
    if (state.variant !== "B") return;
    if (state.askingStep !== null) return;
    // start with step 0
    dispatch({ type: "ask-step", value: 0 });
    // eslint-disable-next-line
  }, [state.phase === "running" && state.variant === "B"]);

  // ---- variant B: execute step then prompt next ----
  const execStepB = useCallback(
    (idx) => {
      dispatch({ type: "step-running", value: idx });
      const step = PLAN.steps[idx];
      const ms = step.takesMs * speedMult(state.speed);
      setTimeout(() => {
        // failure injection
        if (state.failure === "hook" && step.verb === "commit") {
          dispatch({
            type: "step-failed",
            value: idx,
            reason: "pre-commit hook 'cargo fmt --check' failed (exit 1)",
          });
          return;
        }
        if (state.failure === "push" && step.verb === "push") {
          dispatch({
            type: "step-failed",
            value: idx,
            reason:
              "push rejected: non-fast-forward — origin/main has 1 new commit since your last fetch",
          });
          return;
        }
        dispatch({ type: "step-done", value: idx });
        const next = idx + 1;
        if (next >= PLAN.steps.length) {
          dispatch({ type: "all-done" });
        } else {
          setTimeout(() => dispatch({ type: "ask-step", value: next }), 120);
        }
      }, ms);
    },
    [state.failure, state.speed]
  );

  const skipStepB = useCallback(
    (idx) => {
      dispatch({ type: "step-skipped", value: idx });
      const next = idx + 1;
      if (next >= PLAN.steps.length) {
        dispatch({ type: "all-done" });
      } else {
        setTimeout(() => dispatch({ type: "ask-step", value: next }), 120);
      }
    },
    []
  );

  // ---- keyboard handler ----
  useEffect(() => {
    const handler = (e) => {
      // edit modal owns keys
      if (state.editing) {
        if (e.key === "Escape") {
          e.preventDefault();
          dispatch({ type: "edit-cancel" });
        } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
          e.preventDefault();
          dispatch({ type: "edit-save" });
        }
        return;
      }

      // ignore modifier-only keys
      if (e.metaKey || e.ctrlKey || e.altKey) return;

      const key = e.key;
      const lower = key.toLowerCase();

      // ---- IDLE: any key starts ----
      if (state.phase === "idle") {
        if (key === "Enter" || lower === "r" || key === " ") {
          e.preventDefault();
          dispatch({ type: "start" });
        }
        return;
      }

      // ---- THINKING: q cancels ----
      if (state.phase === "thinking") {
        if (lower === "q" || key === "Escape") {
          e.preventDefault();
          dispatch({ type: "cancel" });
        }
        return;
      }

      // ---- DONE / CANCELED: r or Enter resets ----
      if (state.phase === "done" || state.phase === "canceled") {
        if (key === "Enter" || lower === "r") {
          e.preventDefault();
          dispatch({ type: "reset" });
        }
        return;
      }

      // ---- FAILED: u undo (reset), a abandon (cancel), q abort ----
      if (state.phase === "failed") {
        if (lower === "u") {
          e.preventDefault();
          dispatch({ type: "reset" });
        } else if (lower === "a" || lower === "q" || key === "Escape") {
          e.preventDefault();
          dispatch({ type: "cancel" });
        }
        return;
      }

      // ---- PLAN phase ----
      if (state.phase === "plan") {
        if (key === "Enter") {
          e.preventDefault();
          dispatch({ type: "exec-start" });
        } else if (lower === "q" || key === "Escape") {
          e.preventDefault();
          dispatch({ type: "cancel" });
        } else if (lower === "e") {
          // Variant A & C: edit message
          if (state.variant === "A" || state.variant === "C") {
            e.preventDefault();
            dispatch({ type: "edit-open" });
          }
        } else if (state.variant === "C") {
          // focus + skip
          if (key === "ArrowDown" || key === "Tab") {
            e.preventDefault();
            const next = Math.min(state.focused + 1, PLAN.steps.length - 1);
            dispatch({ type: "focus", value: next });
          } else if (key === "ArrowUp") {
            e.preventDefault();
            dispatch({ type: "focus", value: Math.max(state.focused - 1, 0) });
          } else if (lower === "s") {
            e.preventDefault();
            dispatch({ type: "toggle-skip", value: state.focused });
          }
        }
        return;
      }

      // ---- RUNNING + variant B per-step prompt ----
      if (state.phase === "running" && state.variant === "B" && state.askingStep !== null) {
        const idx = state.askingStep;
        const step = PLAN.steps[idx];
        if (lower === "y") {
          e.preventDefault();
          execStepB(idx);
        } else if (lower === "n") {
          e.preventDefault();
          skipStepB(idx);
        } else if (lower === "e" && step.editable) {
          e.preventDefault();
          dispatch({ type: "edit-open" });
        } else if (lower === "q" || key === "Escape") {
          e.preventDefault();
          dispatch({ type: "cancel" });
        }
        return;
      }

      // ---- RUNNING for A/C: q aborts ----
      if (state.phase === "running") {
        if (lower === "q" || key === "Escape") {
          e.preventDefault();
          dispatch({ type: "cancel" });
        }
        return;
      }
    };

    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [state, execStepB, skipStepB]);

  // ---- focus textarea when edit opens ----
  const textareaRef = useRef(null);
  useEffect(() => {
    if (state.editing && textareaRef.current) {
      textareaRef.current.focus();
      textareaRef.current.select();
    }
  }, [state.editing]);

  // ---- phase badge text ----
  const phaseBadge = (() => {
    switch (state.phase) {
      case "idle": return { text: "idle", cls: "" };
      case "thinking": return { text: "thinking", cls: "run" };
      case "plan": return { text: "awaiting confirm", cls: "" };
      case "running": return { text: "running", cls: "run" };
      case "done": return { text: "done", cls: "done" };
      case "failed": return { text: "failed", cls: "failed" };
      case "canceled": return { text: "canceled", cls: "" };
      default: return { text: state.phase, cls: "" };
    }
  })();

  return (
    <div className="app">
      <div className="hdr">
        <h1>Glit · Plan Execution Prototype</h1>
        <p className="sub">
          Three keymap variants for the same plan: <code>{PLAN.command}</code>. Switch variants, inject a failure, and walk through the flow on the keyboard. Use the legend on the right for keymap reminders.
        </p>
      </div>

      <div className="controls">
        <ControlGroup label="Variant">
          <Seg
            value={state.variant}
            options={["A", "B", "C"]}
            labels={{ A: "A · Atomic", B: "B · Per-step", C: "C · Hybrid" }}
            onChange={(v) => dispatch({ type: "set-variant", value: v })}
          />
        </ControlGroup>

        <ControlGroup label="Failure">
          <Seg
            value={state.failure}
            options={["none", "hook", "push"]}
            labels={{ none: "none", hook: "commit-hook fail", push: "push rejected" }}
            onChange={(v) => dispatch({ type: "set-failure", value: v })}
          />
        </ControlGroup>

        <ControlGroup label="Speed">
          <Seg
            value={state.speed}
            options={["fast", "normal", "slow"]}
            labels={{ fast: "fast", normal: "normal", slow: "slow" }}
            onChange={(v) => dispatch({ type: "set-speed", value: v })}
          />
        </ControlGroup>

        <div className="ctrl-spacer" />

        <button className="reset-btn" onClick={() => dispatch({ type: "reset" })}>
          ↻ Reset
        </button>
      </div>

      <div className="main">
        <div className="term">
          <div className="term-head">
            <div className="dots"><i /><i /><i /></div>
            <div className="ttl">terminal · {state.variant}</div>
            <div className={`badge ${phaseBadge.cls}`}>{phaseBadge.text}</div>
          </div>
          <div className="term-body">
            <CommandLine />
            {state.phase !== "idle" && <Stages state={state} />}
            {["plan", "running", "failed", "done", "canceled"].includes(state.phase) && (
              <Plan state={state} dispatch={dispatch} />
            )}
            {state.phase === "plan" && !state.editing && (
              <PlanPrompt state={state} dispatch={dispatch} />
            )}
            {state.phase === "running" && state.variant === "B" && state.askingStep !== null && !state.editing && (
              <StepPrompt state={state} />
            )}
            {state.editing && (
              <EditModal
                value={state.editValue}
                onChange={(v) => dispatch({ type: "edit-change", value: v })}
                onSave={() => dispatch({ type: "edit-save" })}
                onCancel={() => dispatch({ type: "edit-cancel" })}
                inputRef={textareaRef}
              />
            )}
            {state.phase === "failed" && <FailureBlock state={state} />}
            {state.phase === "done" && <DoneBlock />}
            {state.phase === "canceled" && <CanceledBlock />}
            {state.phase === "idle" && <IdleBlock />}
          </div>
        </div>

        <Legend state={state} variant={variant} />
      </div>
    </div>
  );
}

// ============================================================================
// sub-components
// ============================================================================

function ControlGroup({ label, children }) {
  return (
    <div className="ctrl-group">
      <span className="ctrl-label">{label}</span>
      {children}
    </div>
  );
}

function Seg({ value, options, labels, onChange }) {
  return (
    <div className="seg">
      {options.map((opt) => (
        <button
          key={opt}
          className={value === opt ? "active" : ""}
          onClick={() => onChange(opt)}
        >
          {labels[opt]}
        </button>
      ))}
    </div>
  );
}

function CommandLine() {
  return (
    <div>
      <span className="p">$</span> <span className="u">{PLAN.command}</span>
    </div>
  );
}

function Stages({ state }) {
  // Render whichever stages have started
  const visible = STAGES.map((s, i) => {
    const isActive = state.thinkingStage === i;
    const isDone = state.thinkingMs[i] != null;
    if (!isActive && !isDone && state.thinkingStage < i) return null;
    const cls = isDone ? "done" : isActive ? "run" : "";
    return (
      <div key={i} className={`stage ${cls}`}>
        <span className="icon">{isDone ? "✓" : isActive ? "⠋" : "·"}</span>
        <span className="label">
          {isDone ? `${s.label.replace("…", "")}` : `${s.label}…`}
        </span>
        <span className="bud">
          {isDone ? `${state.thinkingMs[i]}ms` : `budget ~${s.budget}ms`}
        </span>
      </div>
    );
  });
  if (state.phase === "thinking" || state.thinkingMs.length > 0) {
    return <div className="stages">{visible}</div>;
  }
  return null;
}

function Plan({ state, dispatch }) {
  return (
    <div className="plan-block">
      <div className="plan-head">
        <span>Plan</span>
        <span className="meta">
          · {PLAN.steps.length} steps · snapshot taken at <span className="y">{PLAN.snapshotSha}</span>
        </span>
      </div>
      <ul className="timeline">
        {PLAN.steps.map((step, i) => (
          <Step
            key={step.id}
            idx={i}
            step={step}
            status={state.stepStatus[i]}
            skipped={state.skipped[i]}
            focused={state.variant === "C" && state.phase === "plan" && state.focused === i}
            message={state.message}
            showFailure={state.phase === "failed" && state.failedStep === i}
            failedReason={state.failedReason}
          />
        ))}
      </ul>
    </div>
  );
}

function Step({ idx, step, status, skipped, focused, message, showFailure, failedReason }) {
  const cls = [
    "t-step",
    skipped ? "skipped" : "",
    status === "running" ? "run" : "",
    status === "done" ? "done" : "",
    status === "skipped" ? "skipped" : "",
    status === "failed" ? "failed" : "",
    focused ? "focused" : "",
  ]
    .filter(Boolean)
    .join(" ");

  const isCommit = step.verb === "commit";
  const verbDisplay = status === "done" ? step.pastVerb : step.verb;
  const marker = step.marker === "caution" ? <span className="marker">⚠</span> : null;

  // delta line on commit/push when done
  const delta = (() => {
    if (status !== "done") return null;
    if (step.verb === "commit")
      return (
        <div className="delta">
          <span className="from">{PLAN.snapshotSha}</span> → <span className="to">{PLAN.resultSha}</span>
        </div>
      );
    if (step.verb === "push")
      return (
        <div className="delta">
          origin/main now at <span className="to">{PLAN.resultSha}</span>
        </div>
      );
    return null;
  })();

  return (
    <li className={cls}>
      <span className="node" />
      <div className="t-card">
        <div className="row">
          <span className="verb">{verbDisplay}</span>
          {isCommit ? (
            <span className="scope">"{message}"</span>
          ) : (
            <span className="target">{step.target}</span>
          )}
          {step.stat && <span className="stat">{step.stat}</span>}
          {marker}
        </div>
        {isCommit && status === "pending" && (
          <div className="meta">
            generated from diff · press <span style={{ color: "var(--accent)" }}>e</span> to edit
          </div>
        )}
        {status === "skipped" && <div className="meta">skipped by user</div>}
        {status === "running" && (
          <div className="meta">
            {step.verb === "stage" && "writing index"}
            {step.verb === "commit" && "writing tree → object store"}
            {step.verb === "push" && "negotiating with origin…"}
          </div>
        )}
        {showFailure && <div className="meta r">{failedReason}</div>}
        {delta}
      </div>
    </li>
  );
}

function PlanPrompt({ state, dispatch }) {
  if (state.variant === "A") {
    return (
      <div className="prompt-row">
        <span className="ask">▸</span>
        <span className="key">Enter</span> run plan
        <span className="sep">·</span>
        <span className="key">e</span> edit message
        <span className="sep">·</span>
        <span className="key">q</span> cancel
      </div>
    );
  }
  if (state.variant === "B") {
    return (
      <div className="prompt-row">
        <span className="ask">▸</span>
        <span className="key">Enter</span> begin (you'll confirm each step)
        <span className="sep">·</span>
        <span className="key">q</span> cancel
      </div>
    );
  }
  // variant C
  const focusedStep = PLAN.steps[state.focused];
  return (
    <div className="prompt-row">
      <span className="ask">▸</span>
      <span className="key">Enter</span> run all
      <span className="sep">·</span>
      <span className="key">↑↓</span> focus
      <span className="sep">·</span>
      {focusedStep.editable && (
        <>
          <span className="key">e</span> edit step {state.focused + 1}
          <span className="sep">·</span>
        </>
      )}
      <span className="key">s</span> {state.skipped[state.focused] ? "unskip" : "skip"} step {state.focused + 1}
      <span className="sep">·</span>
      <span className="key">q</span> cancel
    </div>
  );
}

function StepPrompt({ state }) {
  const idx = state.askingStep;
  const step = PLAN.steps[idx];
  const verbLabel = step.verb === "stage" ? "stage" : step.verb === "commit" ? "commit" : "push";
  return (
    <div className="prompt-row">
      <span className="ask">▸</span>
      Step {idx + 1} · {verbLabel} → run it?
      <span className="sep"> </span>
      <span className="key">y</span> yes
      <span className="sep">·</span>
      <span className="key">n</span> skip
      <span className="sep">·</span>
      {step.editable && (
        <>
          <span className="key">e</span> edit
          <span className="sep">·</span>
        </>
      )}
      <span className="key">q</span> abort
    </div>
  );
}

function EditModal({ value, onChange, onSave, onCancel, inputRef }) {
  return (
    <div className="edit-card">
      <div className="lab">Edit commit message</div>
      <textarea
        ref={inputRef}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        rows={2}
      />
      <div className="hint">
        <span className="k">⌘/Ctrl + Enter</span> save · <span className="k">Esc</span> cancel
      </div>
    </div>
  );
}

function FailureBlock({ state }) {
  const idx = state.failedStep;
  const step = PLAN.steps[idx];
  const isPush = step.verb === "push";
  return (
    <div className="failed-block">
      <div className="err">⚠ Step {idx + 1} failed · {step.verb}</div>
      <div className="detail">{state.failedReason}</div>
      <div className="prompt-row" style={{ border: 0, padding: 0 }}>
        <span className="key">u</span> undo this run · revert to <span className="y" style={{ fontFamily: "Geist Mono, monospace" }}>{PLAN.snapshotSha}</span>
        <span className="sep">·</span>
        <span className="key">a</span> abandon, keep what succeeded
        <span className="sep">·</span>
        <span className="key">q</span> quit
      </div>
      <div className="next">
        Snapshot at <b>{PLAN.snapshotSha}</b> means undo is non-destructive. Working tree was never touched.
      </div>
    </div>
  );
}

function DoneBlock() {
  return (
    <div className="done-block">
      <div className="ok">✓ Done · 1 commit on main, ahead of origin by 0</div>
      <div className="meta" style={{ color: "var(--dim)", fontSize: 12.5 }}>
        Next: <span className="b">glit "status"</span> · undo this with <span className="g">glit undo</span>
      </div>
      <div className="prompt-row" style={{ marginTop: 12 }}>
        <span className="key">r</span> reset prototype · <span className="key">Enter</span> run again
      </div>
    </div>
  );
}

function CanceledBlock() {
  return (
    <div className="canceled-block">
      <div className="x">✗ Canceled · no changes applied</div>
      <div className="meta" style={{ color: "var(--dim)", fontSize: 12.5 }}>
        Snapshot was taken but not used. Working tree unchanged.
      </div>
      <div className="prompt-row" style={{ marginTop: 12 }}>
        <span className="key">r</span> reset · <span className="key">Enter</span> run again
      </div>
    </div>
  );
}

function IdleBlock() {
  return (
    <div className="idle">
      <div className="big">Press <span className="small"><span className="k">Enter</span></span> or <span className="small"><span className="k">r</span></span> to begin.</div>
      <div className="small">
        Switch variant, failure, and speed above. Each variant uses a different keymap — watch the legend.
      </div>
    </div>
  );
}

// ============================================================================
// Legend
// ============================================================================

function Legend({ state, variant }) {
  return (
    <aside className="legend">
      <h3>Variant</h3>
      <div className="variant-name">{variant.name}</div>
      <div className="variant-desc">{variant.desc}</div>

      <h3>Keymap</h3>
      <div className="keymap">
        {variant.keymap.map((row, i) => (
          <div className="row" key={i}>
            <span className={`k${row.danger ? " danger" : ""}`}>{row.k}</span>
            <span>{row.do}</span>
          </div>
        ))}
      </div>

      <h3>Snapshot</h3>
      <div className="snap">
        Pre-run · <span className="sha">{PLAN.snapshotSha}</span>
        <div className="hint">Captures HEAD + index. Working tree never touched. Undo with <span style={{ color: "var(--green)" }}>glit undo</span>.</div>
      </div>

      <h3>Phase</h3>
      <div className={`phase-pill ${state.phase === "running" || state.phase === "thinking" ? "run" : ""} ${state.phase === "done" ? "done" : ""} ${state.phase === "failed" ? "failed" : ""}`}>
        {state.phase}
      </div>

      {state.failure !== "none" && (
        <>
          <h3>Failure injection</h3>
          <div style={{ color: "var(--orange)", fontSize: 12.5, fontFamily: "Geist Mono, monospace" }}>
            ⚠ {state.failure === "hook" ? "pre-commit hook will fail" : "push will be rejected"}
          </div>
        </>
      )}
    </aside>
  );
}

// ============================================================================
// mount
// ============================================================================

const root = ReactDOM.createRoot(document.getElementById("root"));
root.render(<App />);
