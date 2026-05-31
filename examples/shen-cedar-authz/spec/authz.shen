\\ Authorization spec — THE SOURCE OF TRUTH.
\\
\\ The Cedar policy is generated from this file (see `examples/generate.rs`):
\\ Shen computes the transitive grant closure over the role-inheritance DAG,
\\ and the host renders the result to Cedar `permit` statements. Edit grants
\\ and inheritance HERE; never hand-edit the generated Cedar.

\\ base grants — [role action resource]
(defun base-grants () [[Analyst Eval pure] [Auditor Eval logs]])

\\ role inheritance — [role inherits-from]
(defun role-parents () [[Admin Analyst] [Admin Auditor] [Lead Analyst]])

\\ roles to emit policy for
(defun all-roles () [Analyst Auditor Admin Lead])

\\ ---- the closure: a role's grants = its own + every ancestor's ----------
(defun appnd (a b) (if (= a []) b (cons (hd a) (appnd (tl a) b))))
(defun mem (x xs) (if (= xs []) false (if (= x (hd xs)) true (mem x (tl xs)))))

(defun parents-h (r es)
  (if (= es []) []
    (if (= (hd (hd es)) r)
        (cons (hd (tl (hd es))) (parents-h r (tl es)))
      (parents-h r (tl es)))))
(defun parents (r) (parents-h r (role-parents)))

(defun anc-list (rs) (if (= rs []) [] (appnd (ancestors (hd rs)) (anc-list (tl rs)))))
(defun ancestors (r) (cons r (anc-list (parents r))))

(defun gfilter (anc gs r)
  (if (= gs []) []
    (if (mem (hd (hd gs)) anc)
        (cons (cons r (tl (hd gs))) (gfilter anc (tl gs) r))
      (gfilter anc (tl gs) r))))
(defun grants-of (r) (gfilter (ancestors r) (base-grants) r))

(defun expand-roles (rs) (if (= rs []) [] (appnd (grants-of (hd rs)) (expand-roles (tl rs)))))
(defun expand-all () (expand-roles (all-roles)))
