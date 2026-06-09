\\ Scaled, deterministic bench corpus for benches/authz_served.rs.
\\ Data only — the verifier/spec CODE stays the committed
\\ spec/verify.shen + spec/authz.shen; this file scales the role DAG,
\\ policy scopes, and grant tables so `reaches` does real transitive
\\ work. Loaded after authz.shen (redefines its three data defuns) and
\\ compiled into the same AOT overlay module.

(defun bench-edges () [[r0 r8] [r1 r9] [r2 r10] [r3 r11] [r4 r12] [r5 r13] [r6 r14] [r7 r15] [r8 r16] [r9 r17] [r10 r18] [r11 r19] [r12 r20] [r13 r21] [r14 r22] [r15 r23] [r16 r24] [r17 r25] [r18 r26] [r19 r27] [r20 r28] [r21 r29] [r22 r30] [r23 r31] [r24 r32] [r25 r33] [r26 r34] [r27 r35] [r28 r36] [r29 r37] [r30 r38] [r31 r39] [r32 r40] [r33 r41] [r34 r42] [r35 r43] [r36 r44] [r37 r45] [r38 r46] [r39 r47] [r40 r48] [r41 r49] [r42 r50] [r43 r51] [r44 r52] [r45 r53] [r46 r54] [r47 r55] [r48 r56] [r49 r57] [r50 r58] [r51 r59] [r52 r60] [r53 r61] [r54 r62] [r55 r63] [r0 r9] [r1 r10] [r2 r11] [r3 r12] [r4 r13] [r5 r14] [r6 r15]])

(defun bench-forbids () [[[kin r56] [kany none]] [[kin r49] [kin r41]] [[keq r10] [kin r34]] [[kany none] [keq r21]] [[kin r60] [kany none]] [[kin r53] [kin r45]] [[keq r30] [kin r38]] [[kany none] [keq r49]] [[kin r56] [kany none]] [[kin r49] [kin r49]] [[keq r50] [kin r34]] [[kany none] [keq r13]]])

(defun bench-permits () [[[kin r0] [keq r0]] [[keq r1] [kin r5]] [[kin r22] [kany none]] [[kin r3] [keq r9]] [[keq r4] [kin r20]] [[kin r15] [kany none]] [[kin r6] [keq r18]] [[keq r7] [kin r35]] [[kin r8] [kany none]] [[kin r9] [keq r27]] [[keq r10] [kin r2]] [[kin r1] [kany none]] [[kin r12] [keq r36]] [[keq r13] [kin r17]] [[kin r34] [kany none]] [[kin r15] [keq r45]] [[keq r16] [kin r32]] [[kin r27] [kany none]] [[kin r18] [keq r54]] [[keq r19] [kin r47]] [[kin r20] [kany none]] [[kin r21] [keq r63]] [[keq r22] [kin r14]] [[kin r13] [kany none]] [[kin r0] [keq r8]] [[keq r25] [kin r29]] [[kin r6] [kany none]] [[kin r3] [keq r17]] [[keq r28] [kin r44]] [[kin r39] [kany none]] [[kin r6] [keq r26]] [[keq r31] [kin r11]] [[kin r32] [kany none]] [[kin r9] [keq r35]] [[keq r2] [kin r26]] [[kin r25] [kany none]] [[kin r12] [keq r44]] [[keq r5] [kin r41]] [[kin r18] [kany none]] [[kin r15] [keq r53]] [[keq r8] [kin r8]] [[kin r11] [kany none]] [[kin r18] [keq r62]] [[keq r11] [kin r23]] [[kin r4] [kany none]] [[kin r21] [keq r7]] [[keq r14] [kin r38]] [[kin r37] [kany none]]])

\\ Scaled data for the generate-shape query (expand-all): the three
\\ data defuns of spec/authz.shen, redefined at bench scale.
(defun role-parents () (bench-edges))

(defun all-roles () [r0 r1 r2 r3 r4 r5 r6 r7 r8 r9 r10 r11 r12 r13 r14 r15 r16 r17 r18 r19 r20 r21 r22 r23 r24 r25 r26 r27 r28 r29 r30 r31 r32 r33 r34 r35 r36 r37 r38 r39])

(defun base-grants () [[r0 Eval pure] [r13 Read pure] [r26 Write pure] [r39 Eval logs] [r52 Read logs] [r1 Write logs] [r14 Eval io] [r27 Read io] [r40 Write io] [r53 Eval pure] [r2 Read pure] [r15 Write pure] [r28 Eval logs] [r41 Read logs] [r54 Write logs] [r3 Eval io] [r16 Read io] [r29 Write io] [r42 Eval pure] [r55 Read pure] [r4 Write pure] [r17 Eval logs] [r30 Read logs] [r43 Write logs] [r56 Eval io] [r5 Read io] [r18 Write io] [r31 Eval pure] [r44 Read pure] [r57 Write pure]])
