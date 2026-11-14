// (The model has colours of its own: the face is the vertex colour.) "Edges over the model" (the 3D viewer's wireframe): the face is shaded by the engine like the
// solid view's PrincipledMaterial (same lights, same roughness), and near a triangle's edge the
// colour is darkened. The barycentric coordinates come in `UV1` (the mesh's `uv1s`: (1,0), (0,1),
// (0,0) per triangle); the distance to an edge is turned into pixels with `fwidth`, so the line
// is about one pixel wide at every zoom. One draw call, no line geometry.
void MAIN()
{
    vec3 b = vec3(UV1.x, UV1.y, 1.0 - UV1.x - UV1.y);
    vec3 w = fwidth(b);
    vec3 a = smoothstep(vec3(0.0), w * 1.5, b);
    float edge = min(min(a.x, a.y), a.z);
    // (The vertex colour comes out darker here than through PrincipledMaterial's own handling of it;
    // the exponent lifts it to the solid view's level, measured on the PC under Mesa.)
    vec3 face = pow(VAR_COLOR.rgb, vec3(0.65));
    BASE_COLOR = vec4(mix(face * 0.12, face, edge), 1.0);
    ROUGHNESS = 0.55;
    METALNESS = 0.0;
    SPECULAR_AMOUNT = 0.5;
}
