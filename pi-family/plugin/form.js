(function () {
  "use strict";

  const REPLY = "reply", ADDED_REPLY_LABEL = "Instructions", ADDED_REPLY_PLACEHOLDER = "Optional instructions for your agent", SUBMIT = "Submit";

  function formSlot(kind, name) {
    if (kind === "buttons") return "buttons";
    if (kind === "text" && name === REPLY) return "reply";
    return "field";
  }

  function drawnForm(declared) {
    const fields = [];
    const buttons = [];
    let reply = null;
    for (const declaredControl of declared) {
      const control = { ...declaredControl };
      switch (formSlot(control.kind, control.name)) {
        case "buttons":
          buttons.push(control);
          break;
        case "reply":
          if (typeof control.label !== "string" || !control.label) {
            control.label = ADDED_REPLY_LABEL;
            if (typeof control.placeholder !== "string" || !control.placeholder) {
              control.placeholder = ADDED_REPLY_PLACEHOLDER;
            }
            control.multiline = true;
          }
          reply = control;
          break;
        default:
          fields.push(control);
      }
    }
    // Every form has the box; only a raiser's own `reply` is it. Other text fields collect values.
    if (reply) {
      fields.push(reply);
    } else if (!declared.some((control) => control.name === REPLY)) {
      fields.push({
        kind: "text",
        name: REPLY,
        label: ADDED_REPLY_LABEL,
        placeholder: ADDED_REPLY_PLACEHOLDER,
        multiline: true,
        required: false,
      });
    }
    return {
      controls: fields.concat(buttons),
      submit: !buttons.some((control) => control.options?.some((option) => option.stance !== "rejection")),
    };
  }

  function stanceOf(controls, values) {
    for (const control of controls) {
      if (control.kind !== "buttons") continue;
      const option = control.options?.find((candidate) => candidate.id === values[control.name]);
      if (option?.stance === "rejection") return "rejection";
    }
    return "neutral";
  }

  function submissionValues(controls, values, pressed) {
    const submitted = { ...values };
    for (const control of controls) {
      if (control.kind === "buttons") delete submitted[control.name];
    }
    if (pressed) {
      const control = controls.find((candidate) => candidate.kind === "buttons" && candidate.name === pressed.name);
      if (control?.options?.some((option) => option.id === pressed.id)) submitted[pressed.name] = pressed.id;
    }
    return submitted;
  }

  // What a press sends: `controls` drawn by `drawnForm`, `draft` the form as the person left
  // it (an editor's entry is its JSON text), `pressed` the button `{name, id}` or null.
  // An editor answers as `payload` and never as a value; a signer refuses an answer that
  // carries one among its values. A `several` choice is its picked ids, sorted, and `[]`
  // when none was picked; a choice asked of rows is each row's pick (`rowPicks`), `{}` for none.
  function submission(controls, draft, pressed) {
    const values = {};
    let editor = null;
    for (const control of controls) {
      const value = draft[control.name];
      if (control.kind === "edit") {
        if (!editor) editor = control;
      } else if (control.kind === "choice" && hasRows(control)) {
        values[control.name] = rowPicks(control, value);
      } else if (control.kind === "choice" && control.pick === "several") {
        values[control.name] = Array.isArray(value) ? [...value].sort() : [];
      } else if (value !== undefined) {
        values[control.name] = value;
      }
    }
    const answer = { values: submissionValues(controls, values, pressed) };
    if (editor) {
      try {
        answer.payload = JSON.parse(draft[editor.name]);
      } catch (_) {
        throw new Error("Payload must be valid JSON.");
      }
    }
    return answer;
  }

  function missingRequired(controls, values, stance) {
    const reply = String(values[REPLY] || "").trim();
    if (stance === "rejection" || reply) return null;
    const hasNeutralAction = controls.some((control) =>
      control.kind === "buttons" && control.options?.some((option) => option.stance !== "rejection"));
    const pressedAction = controls.some((control) =>
      control.kind === "buttons" && control.options?.some((option) => option.id === values[control.name]));
    for (const control of controls) {
      const value = values[control.name];
      if (control.kind === "buttons" && hasNeutralAction && !pressedAction) return control.label || "an action";
      if (!control.required) continue;
      if (control.kind === "choice" && hasRows(control)) {
        const picked = rowPicks(control, value);
        if (control.rows.some((row) => picked[row.id] === undefined)) return control.label || control.name;
        continue;
      }
      if (control.kind === "choice" && (!value || (Array.isArray(value) && !value.length))) {
        return control.label || control.name;
      }
      if (control.kind === "text" && !String(value || "").trim()) return control.label || control.name;
      if (control.kind === "number" && (typeof value !== "number" || Number.isNaN(value))) {
        return control.label || control.name;
      }
    }
    return null;
  }

  // A choice asked of rows: the same options for each row, one answer each (a table).
  function hasRows(control) {
    return Array.isArray(control?.rows) && control.rows.length > 0;
  }

  // What a choice asked of rows answers: an object from each row it declares to that row's
  // pick, an offered id for `pick: "one"` and a sorted list of them for `"several"`. A row
  // left alone, and anything the form does not offer, is absent. `{}` when nothing is picked.
  function rowPicks(control, value) {
    const picked = {};
    if (!hasRows(control) || !value || typeof value !== "object" || Array.isArray(value)) return picked;
    const offered = new Set((control.options || []).map((option) => option.id));
    for (const row of control.rows) {
      const chosen = value[row.id];
      if (control.pick === "several") {
        const ids = Array.isArray(chosen) ? [...new Set(chosen.filter((id) => offered.has(id)))].sort() : [];
        if (ids.length) picked[row.id] = ids;
      } else if (typeof chosen === "string" && offered.has(chosen)) {
        picked[row.id] = chosen;
      }
    }
    return picked;
  }

  // A press on one option, in one row when the choice is asked of rows: what the choice
  // holds after it. `pick: "one"` takes the option, or lets it go when it was the one taken
  // and the choice is not required; `"several"` adds or takes away. Every surface that draws
  // a choice presses through this, so a tap means one thing everywhere.
  function press(control, value, rowId, optionId) {
    const one = control.pick === "one";
    const flip = (held) => {
      if (one) return held === optionId && !control.required ? undefined : optionId;
      const ids = new Set(Array.isArray(held) ? held : []);
      if (ids.has(optionId)) ids.delete(optionId);
      else ids.add(optionId);
      return [...ids].sort();
    };
    if (!hasRows(control)) return flip(value);
    const next = { ...rowPicks(control, value) };
    const held = flip(next[rowId]);
    if (held === undefined || (Array.isArray(held) && !held.length)) delete next[rowId];
    else next[rowId] = held;
    return next;
  }

  // What a text or choice control starts holding, as its `default` says it: the words, the
  // id picked, the ids ticked, or each row's pick. A copy, so a press never changes the
  // request. Nothing where the control has no default.
  function started(control) {
    const given = control.default;
    if (given === undefined || given === null) return undefined;
    if (control.kind === "text") return typeof given === "string" ? given : undefined;
    if (control.kind !== "choice") return undefined;
    if (hasRows(control)) return rowPicks(control, given);
    if (control.pick === "several") return Array.isArray(given) ? [...given] : [];
    return typeof given === "string" ? given : undefined;
  }

  // What a text box's label says when the box draws it: nothing, when its own placeholder
  // already says those words, since the box says them. `said` is the request's
  // `labels_said`, the names the judge read so when it was raised; the box a surface adds
  // wears the product's own two phrases, written to say one thing. A required box keeps
  // its label, for the mark after it.
  function shownLabel(control, said) {
    const label = typeof control.label === "string" ? control.label : "";
    if (!label || control.required || control.kind !== "text") return label;
    const added = label === ADDED_REPLY_LABEL && control.placeholder === ADDED_REPLY_PLACEHOLDER;
    return added || (Array.isArray(said) && said.includes(control.name)) ? "" : label;
  }

  // How wide every option of a choice is drawn, in characters: its longest label or
  // description, within bounds. Every option gets that one width, and a row holds as many
  // as fit, so short options sit side by side and a long description wraps inside its own.
  const CHOICE_CELL_MIN = 12, CHOICE_CELL_MAX = 32;

  function choiceCell(options) {
    let widest = 0;
    for (const option of options || []) {
      for (const words of [option.label, option.description]) {
        widest = Math.max(widest, [...String(words || "")].length);
      }
    }
    return Math.min(CHOICE_CELL_MAX, Math.max(CHOICE_CELL_MIN, widest));
  }

  // `globalThis`, not `window`: a page and a card read it as `window.steeryForm`, and a
  // React Native client — which has no `window` to rely on — reads the same object.
  globalThis.steeryForm = {
    REPLY, ADDED_REPLY_LABEL, ADDED_REPLY_PLACEHOLDER, SUBMIT, CHOICE_CELL_MIN, CHOICE_CELL_MAX,
    formSlot, drawnForm, stanceOf, submissionValues, submission, missingRequired, shownLabel, choiceCell,
    hasRows, rowPicks, press, started,
  };
})();
