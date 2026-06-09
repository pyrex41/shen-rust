\\ The Shen verifier — hierarchy-aware scope reasoning over Cedar policy
\\ scopes. THE SOURCE OF TRUTH, shared by:
\\   * examples/verify.rs (include_str! + load_source), and
\\   * the authz_served bench's AOT overlay (bootstrap -> klcompile).
\\ Edit the verifier HERE; never fork the defuns into Rust strings.
\\
\\ A scope is encoded [kind id] with kind in {kany, kin, keq}; es is the
\\ membership DAG as [child parent] edges.

(defun parents-of (x es)
  (if (= es []) []
    (if (= (hd (hd es)) x)
        (cons (hd (tl (hd es))) (parents-of x (tl es)))
      (parents-of x (tl es)))))

\\ a reaches b  ==  a is-in b  (a == b, or some parent of a reaches b)
(defun reaches (a b es)
  (if (= a b) true (reaches-list (parents-of a es) b es)))

(defun reaches-list (ps b es)
  (if (= ps []) false
    (if (reaches (hd ps) b es) true (reaches-list (tl ps) b es))))

\\ forbid-scope f COVERS permit-scope p  (f's entity-set ⊇ p's)
(defun s-covers (f p es)
  (let fk (hd f) (let fi (hd (tl f)) (let pk (hd p) (let pi (hd (tl p))
    (if (= fk kany) true
      (if (= fk kin)
          (if (= pk kin) (reaches pi fi es) (if (= pk keq) (reaches pi fi es) false))
        (if (= pk keq) (= fi pi) false))))))))

\\ forbid-scope f INTERSECTS permit-scope p
(defun s-inter (f p es)
  (let fk (hd f) (let fi (hd (tl f)) (let pk (hd p) (let pi (hd (tl p))
    (if (= fk kany) true
      (if (= pk kany) true
        (if (= fk kin)
            (if (= pk kin) (or (reaches fi pi es) (reaches pi fi es)) (reaches pi fi es))
          (if (= pk keq) (= fi pi) (reaches fi pi es))))))))))

(defun classify (fp fr pp pr es)
  (if (and (s-covers fp pp es) (s-covers fr pr es)) shadowed
    (if (and (s-inter fp pp es) (s-inter fr pr es)) overlap disjoint)))

\\ ---- batch driver ---------------------------------------------------
\\ A served verifier sweeps every forbid x permit pair in-language so the
\\ host marshals once per sweep, not once per pair. f and p are policy
\\ scope pairs [principal-scope resource-scope]; the result is the
\\ verdict matrix, one row per forbid.

(defun classify-pair (f p es)
  (classify (hd f) (hd (tl f)) (hd p) (hd (tl p)) es))

(defun classify-row (f ps es)
  (if (= ps []) []
    (cons (classify-pair f (hd ps) es) (classify-row f (tl ps) es))))

(defun classify-all (fs ps es)
  (if (= fs []) []
    (cons (classify-row (hd fs) ps es) (classify-all (tl fs) ps es))))
